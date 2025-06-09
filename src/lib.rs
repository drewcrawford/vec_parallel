/*!
A library for building vectors in parallel using async tasks.

This crate provides utilities to construct `Vec<T>` in parallel by dividing the work into
multiple async tasks that can run concurrently. This is particularly useful for CPU-bound
initialization tasks where elements can be computed independently.

# Example

```
use vec_parallel::{build_vec, Strategy};

// Build a vector of squares using multiple tasks
let builder = build_vec(100, Strategy::TasksPerCore(4), |i| i * i);

// Run the tasks (in a real application, these would be spawned on an executor)
for task in builder.tasks {
    test_executors::spin_on(task);
}

// Get the final result
let squares = test_executors::spin_on(builder.result);
assert_eq!(squares[10], 100); // 10² = 100
```

# Features

- **Flexible parallelization strategies**: Choose how many tasks to create
- **Zero-copy construction**: Elements are written directly to their final location
- **Executor-agnostic**: Works with any async runtime
- **Optional executor integration**: Use the `some_executor` feature for convenient spawning
*/

use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll};
use atomic_waker::AtomicWaker;

/// Determines how work is divided among parallel tasks.
///
/// The strategy controls how many tasks are created to build the vector.
/// Different strategies are optimal for different workloads.
///
/// # Examples
///
/// ```
/// use vec_parallel::{build_vec, Strategy};
///
/// // Use a single task (no parallelism)
/// let builder = build_vec(10, Strategy::One, |i| i * 2);
/// # for task in builder.tasks { test_executors::spin_on(task); }
/// # let result = test_executors::spin_on(builder.result);
///
/// // Use exactly 4 tasks
/// let builder = build_vec(100, Strategy::Tasks(4), |i| i * 2);
/// # for task in builder.tasks { test_executors::spin_on(task); }
/// # let result = test_executors::spin_on(builder.result);
///
/// // Create one task per element (maximum parallelism)
/// let builder = build_vec(10, Strategy::Max, |i| i * 2);
/// # for task in builder.tasks { test_executors::spin_on(task); }
/// # let result = test_executors::spin_on(builder.result);
///
/// // Create 4 tasks per CPU core
/// let builder = build_vec(1000, Strategy::TasksPerCore(4), |i| i * 2);
/// # for task in builder.tasks { test_executors::spin_on(task); }
/// # let result = test_executors::spin_on(builder.result);
/// ```
#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
#[non_exhaustive]
pub enum Strategy {
    /// Creates a single task to build the entire vector.
    ///
    /// This effectively disables parallelism and is equivalent to
    /// building the vector sequentially.
    One,
    /// Creates exactly the specified number of tasks.
    ///
    /// The work is divided evenly among the tasks. If the number of tasks
    /// exceeds the number of elements, it is automatically reduced.
    ///
    /// # Example
    ///
    /// ```
    /// # use vec_parallel::{build_vec, Strategy};
    /// // Create 4 tasks to build a vector of 100 elements
    /// // Each task will handle 25 elements
    /// let builder = build_vec(100, Strategy::Tasks(4), |i| i);
    /// assert_eq!(builder.tasks.len(), 4);
    /// ```
    Tasks(usize),
    /// Creates one task per element (maximum parallelism).
    ///
    /// This strategy provides the finest granularity but may have
    /// higher overhead for simple computations.
    ///
    /// # Example
    ///
    /// ```
    /// # use vec_parallel::{build_vec, Strategy};
    /// // Create 10 tasks for 10 elements
    /// let builder = build_vec(10, Strategy::Max, |i| i);
    /// assert_eq!(builder.tasks.len(), 10);
    /// ```
    Max,
    /// Creates a number of tasks based on the CPU core count.
    ///
    /// The total number of tasks is `cores * multiplier`. This is ideal
    /// for CPU-bound workloads. Common multipliers are 4-8 for balanced
    /// performance.
    ///
    /// # Example
    ///
    /// ```
    /// # use vec_parallel::{build_vec, Strategy};
    /// // On a 4-core system, this creates 16 tasks
    /// let builder = build_vec(1000, Strategy::TasksPerCore(4), |i| i);
    /// let expected_tasks = 4 * num_cpus::get();
    /// assert_eq!(builder.tasks.len(), expected_tasks);
    /// ```
    TasksPerCore(usize),

}

#[derive(Debug)]
struct SharedWaker {
    outstanding_tasks: AtomicUsize,
    waker: AtomicWaker,
}



/// A future that builds a slice of the final vector.
///
/// Each `SliceTask` is responsible for computing and storing elements
/// for a specific range of indices. Multiple tasks can run concurrently
/// to build different parts of the vector in parallel.
///
/// # Example
///
/// ```
/// use vec_parallel::{build_vec, Strategy};
///
/// let builder = build_vec(20, Strategy::Tasks(2), |i| i * 3);
/// 
/// // Each task handles a portion of the vector
/// assert_eq!(builder.tasks.len(), 2);
/// 
/// // Tasks can be polled independently
/// for mut task in builder.tasks {
///     test_executors::spin_on(task);
/// }
/// ```
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SliceTask<T, B> {
    ///A function that builds the value.
    build: B,
    ///An arc that ensures the slice is not deallocated.
    own: Weak<Vec<MaybeUninit<T>>>,
    start: usize,
    past_end: usize,
    shared_waker: Arc<SharedWaker>,
    poison: bool,
}

unsafe impl<T,B> Send for SliceTask<T,B> where T: Send, B: Send {}

impl <T,B> SliceTask<T,B> where B: FnMut(usize) -> T {
    /// Executes this task synchronously, building all elements in its range.
    ///
    /// This method is an alternative to polling the future and is useful when
    /// you want to run tasks on specific threads or in a blocking context.
    ///
    /// # Panics
    ///
    /// Panics if called after the task has already completed.
    ///
    /// # Example
    ///
    /// ```
    /// use vec_parallel::{build_vec, Strategy};
    ///
    /// let mut builder = build_vec(10, Strategy::One, |i| i * 2);
    /// 
    /// // Run the task manually
    /// builder.tasks[0].run();
    /// 
    /// let result = test_executors::spin_on(builder.result);
    /// assert_eq!(result, vec![0, 2, 4, 6, 8, 10, 12, 14, 16, 18]);
    /// ```
    pub fn run(&mut self) {
        assert!(!self.poison, "Polling after completion");
        let own = self.own.upgrade().expect("SliceTask was deallocated");
        for i in self.start..self.past_end {
            unsafe {
                let ptr = own.as_ptr();
                // assuming our slices are nonoverlapping, we can write to the slice
                let ptr = ptr as *mut MaybeUninit<T>;
                ptr.add(i).write(MaybeUninit::new((self.build)(i)));
            }
        }
        let old = self.shared_waker.outstanding_tasks.fetch_sub(1, std::sync::atomic::Ordering::Release);
        if old == 1 {
            self.shared_waker.waker.wake();
        }
        self.poison = true;
    }

    /**
    This is a helper function to run the task in a depinned state.

    # Safety
    This function is unsafe because it assumes that we can write to the slice.  e.g., not deallocated,
    no overlapping writes/reads, etc.
*/
    unsafe fn run_depinned(start: usize, past_end: usize, build: &mut B, vec_base: *mut MaybeUninit<T>, shared_waker: &Arc<SharedWaker>, poison: &mut bool) {
        for i in start..past_end {
            unsafe {
            // assuming our slices are nonoverlapping, we can write to the slice
                let ptr = vec_base.add(i);
                ptr.write(MaybeUninit::new(build(i)));
            }
        }
        let old = shared_waker.outstanding_tasks.fetch_sub(1, std::sync::atomic::Ordering::Release);
        if old == 1 {
            shared_waker.waker.wake();
        }
        *poison = true;
    }
}

impl<T, B> Future for SliceTask<T, B> where B: FnMut(usize) -> T {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.poison, "Polling after completion");
        //pin-project
        let (start, past_end, build, weak_own, shared_waker,poison) = unsafe {
            let s = self.get_unchecked_mut();
            let start = &mut s.start;
            let past_end = &mut s.past_end;
            let build = &mut s.build;
            let weak_own = &mut s.own;
            let shared_waker = &mut s.shared_waker;
            (start, past_end, build, weak_own, shared_waker, &mut s.poison)
        };
        unsafe {
            let safe_arc = weak_own.upgrade().expect("SliceTask was deallocated");
            // assuming our slices are nonoverlapping, we can write to the slice
            let ptr = safe_arc.as_ptr() as *mut MaybeUninit<T>;
            Self::run_depinned(*start, *past_end, build, ptr, shared_waker, poison);

        }
        Poll::Ready(())
    }
}




/// A future that resolves to the completed vector.
///
/// This future waits for all [`SliceTask`]s to complete and then assembles
/// the final vector. It uses atomic operations to track task completion
/// without requiring locks.
///
/// # Example
///
/// ```
/// use vec_parallel::{build_vec, Strategy};
///
/// let builder = build_vec(5, Strategy::Max, |i| format!("Item {}", i));
/// 
/// // Complete all tasks
/// for task in builder.tasks {
///     test_executors::spin_on(task);
/// }
/// 
/// // Get the final vector
/// let result = test_executors::spin_on(builder.result);
/// assert_eq!(result[0], "Item 0");
/// assert_eq!(result[4], "Item 4");
/// ```
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct VecResult<I> {
    vec: Option<Arc<Vec<MaybeUninit<I>>>>,
    shared_waker: Arc<SharedWaker>,
}
impl<I> Future for VecResult<I> {
    type Output = Vec<I>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.shared_waker.waker.register(cx.waker());
        if self.shared_waker.outstanding_tasks.load(std::sync::atomic::Ordering::Acquire) == 0 {
            let v = self.vec.take().expect("Polling after completion");
            let mut v = Arc::into_inner(v).expect("Too many references");
            let ptr = v.as_mut_ptr() as *mut I;
            let len = v.len();
            let cap = v.capacity();
            // SAFETY:
            // 1. We got ptr/len/cap from a valid Vec
            // 2. MaybeUninit<T> and T have the same size and alignment
            // 3. We check for double-free
            let f = unsafe {
                let f = Vec::from_raw_parts(ptr, len, cap);
                std::mem::forget(v);
                f
            };
            Poll::Ready(f)

        } else {
            Poll::Pending
        }
    }
}


/// Contains the tasks and result future for building a vector in parallel.
///
/// A `VecBuilder` is created by [`build_vec`] and contains:
/// - `tasks`: Individual futures that build portions of the vector
/// - `result`: A future that resolves to the completed vector
///
/// # Usage Pattern
///
/// 1. Create a builder with [`build_vec`]
/// 2. Spawn or poll the tasks (potentially on different threads)
/// 3. Await the result to get the final vector
///
/// # Example
///
/// ```
/// use vec_parallel::{build_vec, Strategy};
///
/// async fn build_squares() -> Vec<u32> {
///     let builder = build_vec(10, Strategy::TasksPerCore(2), |i| {
///         // Simulate expensive computation
///         (i as u32) * (i as u32)
///     });
///     
///     // In a real application, spawn tasks on your executor
///     for task in builder.tasks {
///         // tokio::spawn(task);
///         # test_executors::spin_on(task);
///     }
///     
///     // Wait for completion
///     # test_executors::spin_on(builder.result)
///     // builder.result.await
/// }
/// # test_executors::spin_on(build_squares());
/// ```
#[derive(Debug)]
pub struct VecBuilder<I, B> {
    /// The individual tasks that build portions of the vector.
    ///
    /// These can be spawned on an executor or polled manually.
    pub tasks: Vec<SliceTask<I, B>>,
    /// A future that resolves to the completed vector.
    ///
    /// This should be awaited after all tasks have been spawned.
    pub result: VecResult<I>,
}

#[cfg(feature = "some_executor")]
impl <I,B> VecBuilder<I,B> {
    /// Spawns all tasks on the provided executor and awaits the result.
    ///
    /// This is a convenience method that handles task spawning and result
    /// awaiting in one call. The tasks are spawned with the specified
    /// priority and hint.
    ///
    /// # Arguments
    ///
    /// * `executor` - The executor to spawn tasks on
    /// * `priority` - Task priority for scheduling
    /// * `hint` - Execution hint for the executor
    ///
    /// # Example
    ///
    /// ```no_run
    /// # #[cfg(feature = "some_executor")]
    /// # async fn example() {
    /// use vec_parallel::{build_vec, Strategy};
    /// use some_executor::{Priority, hint::Hint};
    ///
    /// let mut executor = /* your executor */;
    /// # let mut executor: Box<dyn some_executor::SomeExecutor> = todo!();
    /// 
    /// let builder = build_vec(100, Strategy::TasksPerCore(4), |i| i * i);
    /// let squares = builder.spawn_on(
    ///     &mut *executor,
    ///     Priority::default(),
    ///     Hint::default()
    /// ).await;
    /// # }
    /// ```
    pub async fn spawn_on<E: some_executor::SomeExecutor>(mut self, executor: &mut E, priority: some_executor::Priority, hint: some_executor::hint::Hint) -> Vec<I>
    where I: Send,
    B: FnMut(usize) -> I,
    B: Send,
    I: 'static,
    B: 'static, {
        use some_executor::task::{ConfigurationBuilder,Task};

        let configuration = ConfigurationBuilder::new()
            .priority(priority)
            .hint(hint)
            .build();

        let mut observers = Vec::with_capacity(self.tasks.len());
        for (t,task) in self.tasks.drain(..).enumerate() {
            let label = format!("VecBuilder task {}",t);
            let t = Task::without_notifications(label, task, configuration.clone());
            let o = executor.spawn(t);
            observers.push(o);
        }
        self.result.await
    }
}

/// Creates a builder for constructing a vector in parallel.
///
/// This function divides the work of building a vector into multiple async tasks
/// based on the specified strategy. Each task computes elements for a range of
/// indices using the provided closure.
///
/// # Arguments
///
/// * `len` - The length of the vector to create
/// * `strategy` - How to divide the work among tasks
/// * `f` - A closure that computes the element at a given index
///
/// # Type Parameters
///
/// * `R` - The element type of the resulting vector
/// * `B` - The closure type (must be `FnMut(usize) -> R` and `Clone`)
///
/// # Examples
///
/// ## Basic usage
///
/// ```
/// use vec_parallel::{build_vec, Strategy};
///
/// // Build a vector of squares
/// let builder = build_vec(10, Strategy::Tasks(2), |i| i * i);
/// 
/// // Execute tasks
/// for task in builder.tasks {
///     test_executors::spin_on(task);
/// }
/// 
/// // Get result
/// let squares = test_executors::spin_on(builder.result);
/// assert_eq!(squares, vec![0, 1, 4, 9, 16, 25, 36, 49, 64, 81]);
/// ```
///
/// ## With expensive computation
///
/// ```
/// use vec_parallel::{build_vec, Strategy};
///
/// fn expensive_computation(n: usize) -> u64 {
///     // Simulate expensive work
///     (0..1000).map(|i| (n + i) as u64).sum()
/// }
///
/// let builder = build_vec(100, Strategy::TasksPerCore(4), expensive_computation);
/// # for task in builder.tasks { test_executors::spin_on(task); }
/// # let result = test_executors::spin_on(builder.result);
/// ```
///
/// ## Stateful closures
///
/// ```
/// use vec_parallel::{build_vec, Strategy};
///
/// let offset = 100;
/// let builder = build_vec(5, Strategy::Max, move |i| i + offset);
/// 
/// # for task in builder.tasks { test_executors::spin_on(task); }
/// let result = test_executors::spin_on(builder.result);
/// assert_eq!(result, vec![100, 101, 102, 103, 104]);
/// ```
pub fn build_vec<'a, R, B>(len: usize, strategy: Strategy, f: B) -> VecBuilder<R, B>
where B: FnMut(usize) -> R,
B: Clone {
    let mut vec = Vec::with_capacity(len);
    vec.resize_with(len, MaybeUninit::uninit);

    let vec_arc = Arc::new(vec);
    match strategy {
        Strategy::One => {
            build_vec(len, Strategy::Tasks(1), f)
        }
        Strategy::Tasks(tasks) => {
            if tasks > len {
                return build_vec(len, Strategy::Tasks(len), f);
            }
            let shared_waker = Arc::new(SharedWaker {
                outstanding_tasks: AtomicUsize::new(tasks),
                waker: AtomicWaker::new(),
            });
            let mut task_vec = Vec::with_capacity(tasks);
            let chunk = len / tasks;
            for i in 0..tasks {
                let start = i * chunk;
                let end = if i + 1 == tasks { len } else { start + chunk };
                let task = SliceTask {
                    build: f.clone(),
                    own: Arc::downgrade(&vec_arc),
                    start,
                    past_end: end,
                    shared_waker: shared_waker.clone(),
                    poison: false,
                };
                task_vec.push(task);
            }
            let result = VecResult { vec: Some(vec_arc), shared_waker };
            VecBuilder {
                tasks: task_vec,
                result,
            }
        }
        Strategy::Max => {
            build_vec(len, Strategy::Tasks(len), f)
        }
        Strategy::TasksPerCore(tasks_per_core) => {
            let tasks = tasks_per_core * num_cpus::get();
            build_vec(len, Strategy::Tasks(tasks), f)
        }
    }
}

//boilerplates

// VecBuilder boilerplate
// Clone: Not implemented. VecBuilder contains mutable tasks with shared synchronization state.
// Cloning would create confusing semantics around task execution and completion tracking.
// May be reconsidered in the future if a clear use case emerges.
//
// PartialEq/Eq: Not implemented. Each VecBuilder represents a unique parallel computation
// with distinct synchronization state. Equality comparisons don't have meaningful semantics.
//
// Send/Sync: Automatically derived based on generic parameters.
// VecBuilder is Send when I: Send and B: Send.
// VecBuilder is Sync when I: Send + Sync and B: Send + Sync.
// This follows from the Send/Sync properties of its fields (tasks and result).
//
// Hash: Not implemented since PartialEq/Eq are not implemented.
// Default: Not implemented. VecBuilder requires specific parameters (length, strategy, closure).
// Display: Not implemented. Not typically useful for builder types.
// From/Into: Not implemented. No obvious conversions to/from other types.
// AsRef/AsMut: Not implemented. Fields are already public, providing direct access.
// Deref/DerefMut: Not implemented. VecBuilder is not a wrapper around a single underlying type.

impl<T,B> PartialEq for SliceTask<T,B> {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.past_end == other.past_end && Weak::ptr_eq(&self.own, &other.own)
    }
}

impl<T,B> Eq for SliceTask<T,B> {}

impl<T,B> std::hash::Hash for SliceTask<T,B> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.start.hash(state);
        self.past_end.hash(state);
        Weak::as_ptr(&self.own).hash(state);
    }
}
//asref/asmut - sort of hard to implement safely to avoid double-muts.

// VecResult boilerplate
// Clone: Not implemented. VecResult is a consuming future that takes ownership of the 
// underlying Vec when polled to completion. Cloning would create confusing semantics 
// where multiple futures try to take ownership of the same data.
//
// PartialEq/Eq: Not implemented. VecResult is a stateful future with internal synchronization
// state that gets consumed during polling. Equality comparisons don't have meaningful semantics.
//
// Hash: Not implemented since PartialEq/Eq are not implemented.
//
// Copy: Not implemented. Contains heap-allocated data via Arc.
//
// Default: Not implemented. VecResult requires specific initialization with a vec and shared_waker
// that coordinate with associated SliceTask instances.
//
// Display: Not implemented. Not typically useful for future types.
//
// From/Into: Not implemented. No obvious conversions to/from other types.
//
// AsRef/AsMut: Not implemented. Internal fields are private implementation details
// of the future's synchronization mechanism.
//
// Deref/DerefMut: Not implemented. VecResult is not a wrapper around a single underlying type.
//
// Send/Sync: Automatically derived based on generic parameter I.
// VecResult is Send when I: Send, and Sync when I: Send + Sync.
// This follows from the Send/Sync properties of Arc<Vec<MaybeUninit<I>>> and Arc<SharedWaker>.

#[cfg(test)]
mod tests {
    use crate::{VecResult};

    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn test_build_vec() {
        let builder = super::build_vec(10, super::Strategy::One, |i: usize| i);
        for task in builder.tasks {
            test_executors::spin_on(task);
        }
        let o = test_executors::spin_on::<VecResult<_>>(builder.result);
        assert_eq!(o, (0..10).collect::<Vec<_>>());
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn test_build_vec_tasks() {
        let builder = super::build_vec(10, super::Strategy::Tasks(2), |i: usize| i);
        assert_eq!(builder.tasks.len(), 2);
        assert_eq!(builder.tasks[0].start, 0);
        assert_eq!(builder.tasks[0].past_end, 5);
        assert_eq!(builder.tasks[1].start, 5);
        assert_eq!(builder.tasks[1].past_end, 10);
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn test_build_vec_tasks_11() {
        let builder = super::build_vec(13, super::Strategy::Tasks(3), |i: usize| i);
        assert_eq!(builder.tasks.len(), 3);
        assert_eq!(builder.tasks[0].start, 0);
        assert_eq!(builder.tasks[0].past_end,4);
        assert_eq!(builder.tasks[1].start, 4);
        assert_eq!(builder.tasks[1].past_end, 8);
        assert_eq!(builder.tasks[2].start, 8);
        assert_eq!(builder.tasks[2].past_end, 13);
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn test_build_vec_tasks_103() {
        let builder = super::build_vec(103, super::Strategy::Tasks(3), |i: usize| i);
        assert_eq!(builder.tasks.len(), 3);
        assert_eq!(builder.tasks[0].start, 0);
        assert_eq!(builder.tasks[0].past_end, 34);
        assert_eq!(builder.tasks[1].start, 34);
        assert_eq!(builder.tasks[1].past_end, 68);
        assert_eq!(builder.tasks[2].start, 68);
        assert_eq!(builder.tasks[2].past_end, 103);
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn test_max() {
        let builder = super::build_vec(10, super::Strategy::Max, |i: usize| i);
        assert_eq!(builder.tasks.len(), 10);
        let mut start = 0;
        for task in builder.tasks {
            assert_eq!(task.start, start);
            assert_eq!(task.past_end, start + 1);
            start += 1;
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn test_too_many_tasks() {
        let builder = super::build_vec(10, super::Strategy::Tasks(20), |i: usize| i);
        assert_eq!(builder.tasks.len(), 10);
        let mut start = 0;
        for task in builder.tasks {
            assert_eq!(task.start, start);
            assert_eq!(task.past_end, start + 1);
            start += 1;
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), test)]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    fn test_tasks_per_core() {
        let builder = super::build_vec(1000, super::Strategy::TasksPerCore(2), |i: usize| i);
        let cpus = num_cpus::get();
        assert_eq!(builder.tasks.len(), 2 * cpus);
        let mut start = 0;
        for task in builder.tasks {
            assert_eq!(task.start, start);
            assert!(task.past_end > start);
            start = task.past_end;
        }
    }

    #[test]
    fn test_send() {
        fn is_send<T: Send>(_t: &T) {}
        fn is_static<T: 'static>(_t: &T) {}

        let mut builder = super::build_vec(1000, super::Strategy::TasksPerCore(2), |i: usize| i);
        let a_task = builder.tasks.remove(0);

        is_send(&a_task);
        is_static(&a_task);

        let a_result = builder.result;
        is_send(&a_result);
        is_static(&a_result);
    }

    #[cfg(feature = "some_executor")]
    #[test] fn test_spawn_on() {
        let executor = test_executors::aruntime::SpawnRuntime;
        some_executor::thread_executor::set_thread_executor(Box::new(executor));
        let builder = super::build_vec(10, super::Strategy::Max, |i: usize| i);

        some_executor::thread_executor::thread_executor(|e| {
            _ = builder.spawn_on(&mut e.unwrap().clone_box(), some_executor::Priority::unit_test(), some_executor::hint::Hint::default());
        });
    }

}
