pub mod a;
pub mod b;

pub trait Greeter {
    fn greet(&self) -> String;
}

pub struct English;
impl Greeter for English {
    fn greet(&self) -> String {
        "hello".to_string()
    }
}

pub struct German;
impl Greeter for German {
    fn greet(&self) -> String {
        "hallo".to_string()
    }
}

pub struct Cache {
    data: Vec<String>,
}

impl Cache {
    pub fn new() -> Cache {
        Cache { data: Vec::new() }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.iter().find(|s| s.as_str() == key).map(|s| s.as_str())
    }
}

pub fn call_via_dyn(g: &dyn Greeter) -> String {
    g.greet()
}

pub fn call_via_generic<T: Greeter>(g: &T) -> String {
    g.greet()
}

pub fn use_cache(c: &Cache) -> Option<&str> {
    let fresh = Cache::new();
    let hit = c.get("x");
    let _ = fresh;
    hit
}
