use std::future::Future;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

enum Strategy {
    /**
    Create a single task.
*/
    One
}

struct SliceTask<'a,T,B> {
    ///The slice to be written to.
    slice: &'a mut [MaybeUninit<T>],
    ///A function that builds the value.
    build: B,
    ///An arc that ensures the slice is not deallocated.
    own: Arc<Vec<MaybeUninit<T>>>,
}

impl<'a,T,B> Future for SliceTask<'a,T,B> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        todo!()
    }
}

trait BuildFn<T> {

    fn build<'a>(&'a mut self, index: usize) -> impl Future<Output=T> + 'a;
}

struct RunFnFuture<'a, F> {
    f: &'a mut F,
    index: usize,
}

impl <F,R> Future for RunFnFuture<'_,F> where F: FnMut(usize) -> R {
    type Output = R;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let index = self.index;
        Poll::Ready((self.f)(index))
    }
}

impl<'a, R,F> BuildFn<R> for F where F: FnMut(usize) -> R + 'a, Self: 'a {

    fn build(&mut self, index: usize) -> RunFnFuture<F> {
        RunFnFuture {
            f: self,
            index,
        }
    }
}



struct VecResult<I> {
    phantom_data: PhantomData<I>
}
impl<I> Future for VecResult<I> {
    type Output = Vec<I>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        todo!()
    }
}

struct VecBuilder<I: 'static,B> {
    tasks: Vec<SliceTask<'static, I,B>>,
    result: VecResult<I>,
}
pub fn build_vec<R,B>(len: usize, strategy: Strategy, f: B) -> VecBuilder<R,B> {
    let mut vec = Vec::with_capacity(len);
    let dangerous_slice = unsafe{std::slice::from_raw_parts_mut(vec.as_mut_ptr() as *mut MaybeUninit<R>, len)};
    let vec_arc = Arc::new(vec);
    match strategy {
        Strategy::One => {
            let task = SliceTask {
                slice: dangerous_slice,
                build: f,
                own: vec_arc,
            };
            let result = VecResult {phantom_data: PhantomData};
            VecBuilder {
                tasks: vec![task],
                result,
            }
        }
    }

}

#[cfg(test)] mod tests {
    use crate::VecResult;

    #[test] fn test_build_vec() {
        let builder = super::build_vec(10, super::Strategy::One, |i: usize| i);
        for task in builder.tasks {
            test_executors::spin_on(task);
        }
        test_executors::spin_on::<VecResult<i32>>(builder.result);
    }
}
