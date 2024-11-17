use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll};
use atomic_waker::AtomicWaker;

pub enum Strategy {
    /**
    Create a single task.
    */
    One,
    /**
    Creates the number of tasks specified.
*/
    Tasks(usize),
    /**
    Creates a maximum number of tasks.
    */
    Max,
    /**
    Creates a number of tasks multiplied by the core count of the system.

    This is useful for CPU-bound tasks.  Generally speaking, values of 5-15 provide good performance.
    */
    TasksPerCore(usize),

}

struct SharedWaker {
    outstanding_tasks: AtomicUsize,
    waker: AtomicWaker,
}



struct SliceTask<T, B> {
    ///A function that builds the value.
    build: B,
    ///An arc that ensures the slice is not deallocated.
    _own: Arc<Vec<MaybeUninit<T>>>,
    vec_base: *mut MaybeUninit<T>,
    start: usize,
    past_end: usize,
    shared_waker: Arc<SharedWaker>,
}

impl<'a, T, B> Future for SliceTask<T, B> where B: FnMut(usize) -> T {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        //pin-project
        let (start, past_end, build, vec_base, shared_waker) = unsafe {
            let s = self.get_unchecked_mut();
            let start = &mut s.start;
            let past_end = &mut s.past_end;
            let build = &mut s.build;
            let vec_base = &mut s.vec_base;
            let shared_waker = &mut s.shared_waker;
            (start, past_end, build, vec_base, shared_waker)
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
        Poll::Ready(())
    }
}






struct VecResult<I> {
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
    tasks: Vec<SliceTask<I, B>>,
    result: VecResult<I>,
}
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

#[cfg(test)]
mod tests {
    use crate::VecResult;

    #[test]
    fn test_build_vec() {
        let builder = super::build_vec(10, super::Strategy::One, |i: usize| i);
        for task in builder.tasks {
            test_executors::spin_on(task);
        }
        let o = test_executors::spin_on::<VecResult<_>>(builder.result);
        assert_eq!(o, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn test_build_vec_tasks() {
        let builder = super::build_vec(10, super::Strategy::Tasks(2), |i: usize| i);
        assert_eq!(builder.tasks.len(), 2);
        assert_eq!(builder.tasks[0].start, 0);
        assert_eq!(builder.tasks[0].past_end, 5);
        assert_eq!(builder.tasks[1].start, 5);
        assert_eq!(builder.tasks[1].past_end, 10);
    }

    #[test]
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

    #[test] fn test_build_vec_tasks_103() {
        let builder = super::build_vec(103, super::Strategy::Tasks(3), |i: usize| i);
        assert_eq!(builder.tasks.len(), 3);
        assert_eq!(builder.tasks[0].start, 0);
        assert_eq!(builder.tasks[0].past_end, 34);
        assert_eq!(builder.tasks[1].start, 34);
        assert_eq!(builder.tasks[1].past_end, 68);
        assert_eq!(builder.tasks[2].start, 68);
        assert_eq!(builder.tasks[2].past_end, 103);
    }

    #[test]
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


}
