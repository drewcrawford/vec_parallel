use std::future::Future;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll};
use atomic_waker::AtomicWaker;

enum Strategy {
    /**
    Create a single task.
    */
    One
}

struct SharedWaker {
    outstanding_tasks: AtomicUsize,
    waker: AtomicWaker,
}



struct SliceTask<T, B> {
    ///A function that builds the value.
    build: B,
    ///An arc that ensures the slice is not deallocated.
    own: Arc<Vec<MaybeUninit<T>>>,
    vec_base: *mut MaybeUninit<T>,
    start: usize,
    past_end: usize,
    shared_waker: Arc<SharedWaker>,
}

impl<'a, T, B> Future for SliceTask<T, B> where B: FnMut(usize) -> T {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        //pin-project
        let (start, past_end, mut build, vec_base, shared_waker) = unsafe {
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

struct VecBuilder<I, B> {
    tasks: Vec<SliceTask<I, B>>,
    result: VecResult<I>,
}
pub fn build_vec<'a, R, B: FnMut(usize) -> R>(len: usize, strategy: Strategy, f: B) -> VecBuilder<R, B> {
    let mut vec = Vec::with_capacity(len);
    let vec_base = vec.as_mut_ptr();
    let vec_arc = Arc::new(vec);
    match strategy {
        Strategy::One => {
            let shared_waker = Arc::new(SharedWaker {
                outstanding_tasks: AtomicUsize::new(1),
                waker: AtomicWaker::new(),
            });
            let task = SliceTask {
                build: f,
                own: vec_arc.clone(),
                vec_base,
                start: 0,
                past_end: len,
                shared_waker: shared_waker.clone(),
            };
            let result = VecResult { vec: Some(vec_arc), shared_waker };
            VecBuilder {
                tasks: vec![task],
                result,
            }
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
        test_executors::spin_on::<VecResult<_>>(builder.result);
    }
}
