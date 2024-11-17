/*!
This library provides a way to build a vector in parallel.
*/

use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll};
use atomic_waker::AtomicWaker;

#[derive(Debug,Clone,Copy,PartialEq,Eq,Hash)]
#[non_exhaustive]
pub enum Strategy {
    /**
    Create a single task.
    */
    One,
    /**
    Creates the number of tasks specified.

    If the number of tasks is greater than the number of elements, the number of tasks is reduced to the number of elements.
*/
    Tasks(usize),
    /**
    Creates a maximum number of tasks.
    */
    Max,
    /**
    Creates a number of tasks multiplied by the core count of the system.

    This is useful for CPU-bound tasks.  Generally speaking, values of 5-15 provide good performance.

    If the number of tasks is greater than the number of elements, the number of tasks is reduced to the number of elements.
    */
    TasksPerCore(usize),

}

#[derive(Debug)]
struct SharedWaker {
    outstanding_tasks: AtomicUsize,
    waker: AtomicWaker,
}



#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SliceTask<T, B> {
    ///A function that builds the value.
    build: B,
    ///An arc that ensures the slice is not deallocated.
    _own: Arc<Vec<MaybeUninit<T>>>,
    vec_base: *mut MaybeUninit<T>,
    start: usize,
    past_end: usize,
    shared_waker: Arc<SharedWaker>,
    poison: bool,
}

unsafe impl<T,B> Send for SliceTask<T,B> where T: Send, B: Send {}

impl <T,B> SliceTask<T,B> where B: FnMut(usize) -> T {
    pub fn run(&mut self) {
        assert!(!self.poison, "Polling after completion");
        for i in self.start..self.past_end {
            unsafe {
                let ptr = self.vec_base.add(i);
                ptr.write(MaybeUninit::new((self.build)(i)));
            }
        }
        let old = self.shared_waker.outstanding_tasks.fetch_sub(1, std::sync::atomic::Ordering::Release);
        if old == 1 {
            self.shared_waker.waker.wake();
        }
        self.poison = true;
    }
}

impl<T, B> Future for SliceTask<T, B> where B: FnMut(usize) -> T {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(!self.poison, "Polling after completion");
        //pin-project
        let (start, past_end, build, vec_base, shared_waker,poison) = unsafe {
            let s = self.get_unchecked_mut();
            let start = &mut s.start;
            let past_end = &mut s.past_end;
            let build = &mut s.build;
            let vec_base = &mut s.vec_base;
            let shared_waker = &mut s.shared_waker;
            (start, past_end, build, vec_base, shared_waker, &mut s.poison)
        };
        for i in *start..*past_end {
            unsafe {
                let ptr = vec_base.add(i);
                ptr.write(MaybeUninit::new(build(i)));
            }
        }
        let old = shared_waker.outstanding_tasks.fetch_sub(1, std::sync::atomic::Ordering::Release);
        if old == 1 {
            shared_waker.waker.wake();
        }
        *poison = true;
        Poll::Ready(())
    }
}




/**
Represents the overall result, or the final vector.
*/

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



pub struct VecBuilder<I, B> {
    pub tasks: Vec<SliceTask<I, B>>,
    pub result: VecResult<I>,
}

/**
Builds a vector in parallel.

# Example
```
use vec_parallel::*;
let builder = build_vec(10, vec_parallel::Strategy::Max, |i: usize| i);
//run the invididual tasks.  Pro tip, these can be dispatched in parallel.
for task in builder.tasks {
    test_executors::spin_on(task);
}
//wait for the result
let o = test_executors::spin_on(builder.result);
assert_eq!(o, (0..10).collect::<Vec<_>>());
```
*/
pub fn build_vec<'a, R, B>(len: usize, strategy: Strategy, f: B) -> VecBuilder<R, B>
where B: FnMut(usize) -> R,
B: Clone {
    let mut vec = Vec::with_capacity(len);
    vec.resize_with(len, MaybeUninit::uninit);

    let vec_base = vec.as_mut_ptr();
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
                    _own: vec_arc.clone(),
                    vec_base,
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

impl<T,B> PartialEq for SliceTask<T,B> {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.past_end == other.past_end && Arc::ptr_eq(&self._own, &other._own)
    }
}

impl<T,B> Eq for SliceTask<T,B> {}

impl<T,B> std::hash::Hash for SliceTask<T,B> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.start.hash(state);
        self.past_end.hash(state);
        Arc::as_ptr(&self._own).hash(state);
    }
}
//asref/asmut - sort of hard to implement safely to avoid double-muts.



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


}
