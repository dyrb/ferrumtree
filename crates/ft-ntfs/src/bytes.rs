//! Little-endian field readers. Bounds-checked: the checks are cheap next to I/O and keep
//! malformed on-disk data from turning into undefined behaviour.

#[inline(always)]
pub fn u8at(b: &[u8], o: usize) -> u8 {
    b[o]
}
#[inline(always)]
pub fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
#[inline(always)]
pub fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
#[inline(always)]
pub fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
