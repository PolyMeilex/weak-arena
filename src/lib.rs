use std::{
    alloc::{self, Layout},
    cell::Cell,
    ops::{Deref, DerefMut},
    ptr::NonNull,
    rc::Rc,
};

/// Store drop function ptr for types that `mem::needs_drop`
struct DropHandler {
    value: NonNull<()>,
    drop: unsafe fn(NonNull<()>),
}

impl DropHandler {
    fn new<T>(value: NonNull<T>) -> Self {
        unsafe fn drop<T>(ptr: NonNull<()>) {
            std::ptr::drop_in_place(ptr.cast::<T>().as_ptr());
        }

        Self {
            value: value.cast(),
            drop: drop::<T>,
        }
    }
}

impl Drop for DropHandler {
    fn drop(&mut self) {
        unsafe { (self.drop)(self.value) };
    }
}

struct AllocationPage {
    layout: Layout,
    start: NonNull<u8>,
    end: NonNull<u8>,
}

impl AllocationPage {
    fn new(size: usize) -> Self {
        let layout = Layout::from_size_align(size, 1).unwrap();

        // SAFETY: layout must have non-zero size. Attempting to allocate for a zero-sized layout may result in undefined behavior.
        let start = unsafe {
            assert_ne!(layout.size(), 0);
            alloc::alloc(layout)
        };

        let start = NonNull::new(start).unwrap();
        let end = unsafe { start.add(size) };

        Self { layout, start, end }
    }

    fn try_alloc_layout(
        &mut self,
        cursor: NonNull<u8>,
        layout: Layout,
    ) -> Option<(NonNull<u8>, NonNull<u8>)> {
        unsafe {
            let align_offset = cursor.align_offset(layout.align());
            let data_ptr = cursor.add(align_offset);

            let data_end_ptr = data_ptr.add(layout.size());

            if data_end_ptr <= self.end {
                Some((data_ptr, data_end_ptr))
            } else {
                None
            }
        }
    }
}

impl Drop for AllocationPage {
    fn drop(&mut self) {
        unsafe {
            alloc::dealloc(self.start.as_ptr(), self.layout);
        }
    }
}

struct Cursor {
    page: usize,
    offset: NonNull<u8>,
}

pub struct WeakArena {
    page_size: usize,
    // TODO: This Vec introduces extra allocation, that could be part of the page allocation itself
    pages: Vec<AllocationPage>,

    cursor: Cursor,
    drop_handlers: Vec<DropHandler>,

    alive: Rc<Cell<bool>>,
}

impl Drop for WeakArena {
    fn drop(&mut self) {
        self.clear();
    }
}

impl WeakArena {
    pub fn new(page_size: usize) -> Self {
        let page = AllocationPage::new(page_size);

        Self {
            page_size,
            cursor: Cursor {
                page: 0,
                offset: page.start,
            },
            pages: vec![page],
            drop_handlers: Vec::new(),
            alive: Rc::new(Cell::new(true)),
        }
    }

    pub fn clear(&mut self) {
        // This will call all `Drop::drop` functions
        self.drop_handlers.clear();

        // Deallocate all pages except the last one
        if self.pages.len() > 1 {
            self.pages.drain(0..self.pages.len() - 1);
        }

        self.alive.set(false);
        self.alive = Rc::new(Cell::new(true));

        self.cursor = Cursor {
            page: 0,
            offset: self.pages[0].start,
        };
    }

    pub fn alloc<T>(&mut self, v: T) -> WeakBox<T> {
        self.alloc_with(|| v)
    }

    pub fn alloc_with<T>(&mut self, f: impl FnOnce() -> T) -> WeakBox<T> {
        /// LLVM optimalisation trick, this helps LLVM understand that
        /// stack allocation is unnecessary
        #[inline(always)]
        unsafe fn write_that_optimizes_nicely<T>(ptr: *mut T, f: impl FnOnce() -> T) {
            std::ptr::write(ptr, f());
        }

        let data_layout = Layout::new::<T>();
        let data_ptr = self.alloc_layout(data_layout).cast::<T>();

        unsafe { write_that_optimizes_nicely(data_ptr.as_ptr(), f) };

        if std::mem::needs_drop::<T>() {
            self.drop_handlers.push(DropHandler::new(data_ptr));
        }

        WeakBox::new(data_ptr, self.alive.clone())
    }

    #[inline(always)]
    fn alloc_in_current_page(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let (data_ptr, data_end_ptr) = self
            .pages
            .get_mut(self.cursor.page)?
            .try_alloc_layout(self.cursor.offset, layout)?;

        self.cursor.offset = data_end_ptr;

        Some(data_ptr)
    }

    #[inline(always)]
    fn alloc_in_new_page(&mut self, layout: Layout) -> NonNull<u8> {
        // Each page twice as big as previous (like Vec)
        self.page_size *= 2;
        // If page size is to small, let's just allocate as much as we need
        self.page_size = self.page_size.max(layout.size());

        let mut page = AllocationPage::new(self.page_size);
        let (data_ptr, data_end_ptr) = page.try_alloc_layout(page.start, layout).unwrap();

        let id = self.pages.len();
        self.pages.push(page);
        self.cursor.page = id;
        self.cursor.offset = data_end_ptr;

        data_ptr
    }

    fn alloc_layout(&mut self, layout: Layout) -> NonNull<u8> {
        self.alloc_in_current_page(layout)
            .unwrap_or_else(|| self.alloc_in_new_page(layout))
    }
}

pub struct WeakBox<T> {
    ptr: NonNull<T>,
    alive: Rc<Cell<bool>>,
}

impl<T> WeakBox<T> {
    pub fn new(ptr: NonNull<T>, alive: Rc<Cell<bool>>) -> Self {
        Self { ptr, alive }
    }

    #[inline]
    pub fn as_ref(&self) -> Option<&T> {
        self.alive.get().then_some(unsafe { self.ptr.as_ref() })
    }

    #[inline]
    pub fn as_mut(&mut self) -> Option<&mut T> {
        self.alive.get().then_some(unsafe { self.ptr.as_mut() })
    }

    pub fn into_shared(self) -> WeakShared<T> {
        WeakShared {
            ptr: self.ptr,
            alive: self.alive,
        }
    }
}

impl<T> Deref for WeakBox<T> {
    type Target = T;

    #[inline(always)]
    #[track_caller]
    fn deref(&self) -> &Self::Target {
        self.as_ref().expect("Dead resource")
    }
}

impl<T> DerefMut for WeakBox<T> {
    #[inline(always)]
    #[track_caller]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut().expect("Dead resource")
    }
}

pub struct WeakShared<T> {
    ptr: NonNull<T>,
    alive: Rc<Cell<bool>>,
}

impl<T> WeakShared<T> {
    pub fn new(ptr: NonNull<T>, alive: Rc<Cell<bool>>) -> Self {
        Self { ptr, alive }
    }

    #[inline]
    pub fn as_ref(&self) -> Option<&T> {
        self.alive.get().then_some(unsafe { self.ptr.as_ref() })
    }
}

impl<T> Deref for WeakShared<T> {
    type Target = T;

    #[inline(always)]
    #[track_caller]
    fn deref(&self) -> &Self::Target {
        self.as_ref().expect("Dead resource")
    }
}

// Clonable because mut access is not possible for Shared
impl<T> Clone for WeakShared<T> {
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr,
            alive: self.alive.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_grows_exp() {
        let mut arena = WeakArena::new(std::mem::size_of::<i32>());

        arena.alloc(0);
        assert_eq!(arena.pages.len(), 1);

        for _ in 0..2 {
            arena.alloc(0);
            assert_eq!(arena.pages.len(), 2);
        }

        for _ in 0..4 {
            arena.alloc(0);
            assert_eq!(arena.pages.len(), 3);
        }

        for _ in 0..8 {
            arena.alloc(0);
            assert_eq!(arena.pages.len(), 4);
        }

        arena.alloc(0);
        assert_eq!(arena.pages.len(), 5);
    }

    #[test]
    fn clear_reuses_largest_page() {
        let mut arena = WeakArena::new(std::mem::size_of::<i32>());

        arena.alloc(0);
        assert_eq!(arena.pages.len(), 1);

        for _ in 0..2 {
            arena.alloc(0);
            assert_eq!(arena.pages.len(), 2);
        }

        for _ in 0..4 {
            arena.alloc(0);
            assert_eq!(arena.pages.len(), 3);
        }

        arena.clear();

        for _ in 0..4 {
            arena.alloc(0);
            assert_eq!(arena.pages.len(), 1);
        }

        arena.alloc(0);
        assert_eq!(arena.pages.len(), 2);
    }

    #[test]
    fn it_works() {
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

        assert!(a.as_ref().is_none());
    }
}
