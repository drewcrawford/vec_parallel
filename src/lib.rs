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

struct SliceTask<T,B> {
    ///A function that builds the value.
    build: B,
    ///An arc that ensures the slice is not deallocated.
    own: Arc<Vec<MaybeUninit<T>>>,
    start: *const MaybeUninit<T>,
    len: usize,
}

impl<T,B> Future for SliceTask<T,B> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        todo!()
    }
}

trait BuildFn<T> {
    type BuildFut<'a>: Future<Output=T> where Self: 'a;
    fn build<'a>(&'a mut self, index: usize) -> Self::BuildFut<'a> where Self: 'a;
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

impl<R,F> BuildFn<R> for F where F: FnMut(usize) -> R {

    type BuildFut<'a> = RunFnFuture<'a,F> where F:'a ;
    fn build<'a>(&'a mut self, index: usize) -> Self::BuildFut<'a>
    where
        Self: 'a,
    {
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
    tasks: Vec<SliceTask<I,B>>,
    result: VecResult<I>,
}
pub fn build_vec<R,B>(len: usize, strategy: Strategy, f: B) -> VecBuilder<R,B> {
    let mut vec = Vec::with_capacity(len);
    let start = vec.as_mut_ptr();
    let vec_arc = Arc::new(vec);
    match strategy {
        Strategy::One => {
            let task = SliceTask {
                build: f,
                own: vec_arc,
                start,
                len,
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
