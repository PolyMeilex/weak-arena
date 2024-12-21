Rust bump arena allocator with weak references.

This has similar haracteristics as `bumpalo` but uses weak references instead of lifetimes.

```rs
let mut arena = WeakArena::new(std::mem::size_of::<i32>());

let a = arena.alloc(10);
let b = arena.alloc(20);
let c = arena.alloc(30);
let d = arena.alloc(40);

assert_eq!(*a, 10);
assert_eq!(*b, 20);
assert_eq!(*c, 30);
assert_eq!(*d, 40);

arena.clear();

assert!(a.as_ref().is_none(), "A is no longer alive");
```
