use std::collections::HashMap;

struct Cache {
    data: HashMap<String, User>,
}

impl Cache {
    fn get(&self, key: &str) -> Option<&User> {
        self.data.get(key)
    }
}

fn main() {
    let c = Cache::new();
    c.get("alice");
}
