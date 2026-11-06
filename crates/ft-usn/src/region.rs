//! One catalog section: either owned heap memory or a slice of a writable file mapping.
//!
//! A mapped section lives in the cache file's own pages: they are file-backed, not
//! private memory, so the OS can drop them under pressure without touching the page
//! file, and writes land in the file directly. Growing past the space reserved in the
//! file moves the section to the heap (one copy); the next save then rewrites the file.

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

enum Inner<T> {
    Heap(Vec<T>),
    Mapped { ptr: NonNull<T>, len: usize, cap: usize },
}

pub struct Region<T> {
    inner: Inner<T>,
}

// SAFETY: a mapped region is uniquely owned by its catalog and only mutated through
// `&mut self`, exactly like a Vec.
unsafe impl<T: Send> Send for Region<T> {}
unsafe impl<T: Sync> Sync for Region<T> {}

impl<T> Default for Region<T> {
    fn default() -> Self {
        Region { inner: Inner::Heap(Vec::new()) }
    }
}

impl<T> From<Vec<T>> for Region<T> {
    fn from(v: Vec<T>) -> Self {
        Region { inner: Inner::Heap(v) }
    }
}

impl<T: Copy> Region<T> {
    /// # Safety
    /// `ptr` must be aligned for `T` and valid for reads and writes of `cap` elements for
    /// as long as this region is mapped (the owner detaches it before unmapping).
    pub(crate) unsafe fn mapped(ptr: *mut T, len: usize, cap: usize) -> Self {
        assert!(len <= cap && (ptr as usize).is_multiple_of(align_of::<T>()));
        Region { inner: Inner::Mapped { ptr: NonNull::new(ptr).expect("mapped section"), len, cap } }
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self.inner, Inner::Mapped { .. })
    }

    pub fn capacity(&self) -> usize {
        match &self.inner {
            Inner::Heap(v) => v.capacity(),
            Inner::Mapped { cap, .. } => *cap,
        }
    }

    /// Heap bytes owned (mapped sections own none).
    pub fn heap_bytes(&self) -> usize {
        match &self.inner {
            Inner::Heap(v) => v.capacity() * size_of::<T>(),
            Inner::Mapped { .. } => 0,
        }
    }

    /// Move a mapped section to the heap with room for `extra` more elements.
    fn move_to_heap(&mut self, extra: usize) {
        if let Inner::Mapped { ptr, len, .. } = self.inner {
            let mut v = Vec::with_capacity(len + extra);
            // SAFETY: valid for `len` elements while mapped.
            v.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr.as_ptr(), len) });
            self.inner = Inner::Heap(v);
        }
    }

    /// Make room for `extra` more elements, leaving the mapping if it is too small.
    pub fn reserve(&mut self, extra: usize) {
        let grow = match &mut self.inner {
            Inner::Heap(v) => {
                v.reserve(extra);
                None
            }
            Inner::Mapped { len, cap, .. } => (*len + extra > *cap).then(|| (*len + extra) / 8 + extra),
        };
        if let Some(g) = grow {
            self.move_to_heap(g);
        }
    }

    pub fn push(&mut self, x: T) {
        self.reserve(1);
        match &mut self.inner {
            Inner::Heap(v) => v.push(x),
            Inner::Mapped { ptr, len, .. } => {
                // SAFETY: `reserve` guaranteed len < cap.
                unsafe { ptr.as_ptr().add(*len).write(x) };
                *len += 1;
            }
        }
    }

    pub fn extend_from_slice(&mut self, s: &[T]) {
        self.reserve(s.len());
        match &mut self.inner {
            Inner::Heap(v) => v.extend_from_slice(s),
            Inner::Mapped { ptr, len, .. } => {
                // SAFETY: `reserve` guaranteed room for `s.len()` more; `s` cannot alias the
                // unused tail of the section.
                unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), ptr.as_ptr().add(*len), s.len()) };
                *len += s.len();
            }
        }
    }

    pub fn truncate(&mut self, n: usize) {
        match &mut self.inner {
            Inner::Heap(v) => v.truncate(n),
            Inner::Mapped { len, .. } => *len = (*len).min(n),
        }
    }

    pub fn resize(&mut self, n: usize, value: T) {
        let len = self.len();
        if n <= len {
            self.truncate(n);
            return;
        }
        self.reserve(n - len);
        for _ in len..n {
            self.push(value);
        }
    }

    /// Replace the contents (stays mapped when it fits).
    pub fn set(&mut self, s: &[T]) {
        self.truncate(0);
        self.extend_from_slice(s);
    }
}

impl<T> Deref for Region<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match &self.inner {
            Inner::Heap(v) => v,
            // SAFETY: valid for `len` elements while mapped.
            Inner::Mapped { ptr, len, .. } => unsafe { std::slice::from_raw_parts(ptr.as_ptr(), *len) },
        }
    }
}

impl<T> DerefMut for Region<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match &mut self.inner {
            Inner::Heap(v) => v,
            // SAFETY: uniquely owned and valid for `len` elements while mapped.
            Inner::Mapped { ptr, len, .. } => unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), *len) },
        }
    }
}

impl<'a, T> IntoIterator for &'a Region<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut Region<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<T: PartialEq> PartialEq for Region<T> {
    fn eq(&self, o: &Self) -> bool {
        **self == **o
    }
}

impl<T: fmt::Debug> fmt::Debug for Region<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_grows_into_the_heap_when_full() {
        let mut backing = vec![0u32; 8];
        // SAFETY: `backing` outlives the region and holds 8 elements.
        let mut r = unsafe { Region::mapped(backing.as_mut_ptr(), 2, 4) };
        r[0] = 7;
        r.push(9);
        assert!(r.is_mapped());
        r.extend_from_slice(&[1]);
        assert!(r.is_mapped(), "exactly fills the capacity");
        r.push(5);
        assert!(!r.is_mapped(), "moved to the heap");
        assert_eq!(&r[..], &[7, 0, 9, 1, 5]);
        assert_eq!(&backing[..4], &[7, 0, 9, 1], "earlier writes went to the mapping");
        r.truncate(1);
        r.set(&[3, 4]);
        assert_eq!(&r[..], &[3, 4]);
    }
}
