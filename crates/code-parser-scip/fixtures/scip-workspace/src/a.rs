pub struct A {
    pub value: i32,
}

impl A {
    pub fn make(v: i32) -> A {
        A { value: v }
    }

    pub fn double(&self) -> i32 {
        self.value * 2
    }
}

pub fn a_fn() -> i32 {
    7
}
