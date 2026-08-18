use crate::a::{A, a_fn};

pub fn b_uses_a() -> i32 {
    let a = A::make(21);
    a.double() + a_fn()
}
