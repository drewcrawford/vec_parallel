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

struct VecTask {

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

struct VecBuilder<I> {
    tasks: Vec<VecTask>,
    result: VecResult<I>,
}
pub fn build_vec<R>(len: usize, strategy: Strategy, f: impl FnMut(usize) -> R) -> VecBuilder<R> {

    match strategy {
        Strategy::One => {
            let task = VecTask {};
            let result = VecResult {phantom_data: PhantomData};
            VecBuilder {
                tasks: vec![task],
                result,
            }
        }
    }

}

#[cfg(test)] mod tests {
    #[test] fn test_build_vec() {
        let builder = super::build_vec(10, super::Strategy::One, |i| i);
        test_executors::spin_on(builder.result);
    }
}
