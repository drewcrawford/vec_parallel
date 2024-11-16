use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

enum Strategy {
    /**
    Create a single task.
*/
    One
}

struct SliceTask<'a,T,B> {
    slice: &'a mut [T],
    build: B,
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
    match strategy {
        Strategy::One => {
            let task = SliceTask {
                slice: &mut vec,
                build: f,
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
            todo!()
        }
        test_executors::spin_on::<VecResult<i32>>(builder.result);
    }
}
