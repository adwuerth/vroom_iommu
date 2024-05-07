use lazy_static::lazy_static;

use std::slice;
// use std::rc::Rc;
// use std::cell::RefCell;
use crate::vfio::*;
use std::collections::HashMap;
use std::error::Error;
use std::io::{self, Read, Seek};
use std::ops::{Deref, DerefMut, Index, IndexMut, Range, RangeFull, RangeTo};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::{fs, mem, process, ptr};
// from https://www.kernel.org/doc/Documentation/x86/x86_64/mm.txt
const X86_VA_WIDTH: u8 = 47;

const HUGE_PAGE_BITS: u32 = 21;
pub const HUGE_PAGE_SIZE: usize = 1 << HUGE_PAGE_BITS;

// pub const IOVA_WIDTH: u8 = X86_VA_WIDTH;
pub const IOVA_WIDTH: u8 = 39;

static HUGEPAGE_ID: AtomicUsize = AtomicUsize::new(0);

pub(crate) static mut VFIO_CONTAINER_FILE_DESCRIPTOR: Option<RawFd> = None;
// pub(crate) static mut VFIO: Option<Vfio> = None;

lazy_static! {
    pub(crate) static ref VFIO_GROUP_FILE_DESCRIPTORS: Mutex<HashMap<i32, RawFd>> =
        Mutex::new(HashMap::new());
}

#[derive(Debug)]
pub struct Dma<T> {
    pub virt: *mut T,
    pub phys: usize,
    pub size: usize,
}

// should be safe
impl<T> Deref for Dma<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.virt }
    }
}

impl<T> DerefMut for Dma<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.virt }
    }
}

// Trait for types that can be viewed as DMA slices
pub trait DmaSlice {
    type Item;

    fn chunks(&self, bytes: usize) -> DmaChunks<u8>;
    fn slice(&self, range: Range<usize>) -> Self::Item;
}

// mildly overengineered lol
pub struct DmaChunks<'a, T> {
    current_offset: usize,
    chunk_size: usize,
    dma: &'a Dma<T>,
}

impl<'a, T> Iterator for DmaChunks<'a, T> {
    type Item = DmaChunk<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_offset >= self.dma.size {
            None
        } else {
            let chunk_phys_addr = self.dma.phys + self.current_offset * std::mem::size_of::<T>();
            let offset_ptr = unsafe { self.dma.virt.add(self.current_offset) };
            let len = std::cmp::min(
                self.chunk_size,
                (self.dma.size - self.current_offset) / std::mem::size_of::<T>(),
            );

            self.current_offset += len;

            Some(DmaChunk {
                phys_addr: chunk_phys_addr,
                slice: unsafe { std::slice::from_raw_parts_mut(offset_ptr, len) },
            })
        }
    }
}

// Represents a chunk obtained from a Dma<T>, with physical address and slice.
pub struct DmaChunk<'a, T> {
    pub phys_addr: usize,
    pub slice: &'a mut [T],
}

impl DmaSlice for Dma<u8> {
    type Item = Dma<u8>;
    fn chunks(&self, bytes: usize) -> DmaChunks<u8> {
        DmaChunks {
            current_offset: 0,
            chunk_size: bytes,
            dma: self,
        }
    }

    fn slice(&self, index: Range<usize>) -> Self::Item {
        assert!(index.end <= self.size, "Index out of bounds");

        unsafe {
            Dma {
                virt: self.virt.add(index.start),
                phys: self.phys + index.start,
                size: (index.end - index.start),
            }
        }
    }
}

impl Index<Range<usize>> for Dma<u8> {
    type Output = [u8];

    fn index(&self, index: Range<usize>) -> &Self::Output {
        assert!(index.end <= self.size, "Index out of bounds");

        unsafe { slice::from_raw_parts(self.virt.add(index.start), index.end - index.start) }
    }
}

impl IndexMut<Range<usize>> for Dma<u8> {
    fn index_mut(&mut self, index: Range<usize>) -> &mut Self::Output {
        assert!(index.end <= self.size, "Index out of bounds");
        unsafe { slice::from_raw_parts_mut(self.virt.add(index.start), index.end - index.start) }
    }
}

impl Index<RangeTo<usize>> for Dma<u8> {
    type Output = [u8];

    fn index(&self, index: RangeTo<usize>) -> &Self::Output {
        &self[0..index.end]
    }
}

impl IndexMut<RangeTo<usize>> for Dma<u8> {
    fn index_mut(&mut self, index: RangeTo<usize>) -> &mut Self::Output {
        &mut self[0..index.end]
    }
}

impl Index<RangeFull> for Dma<u8> {
    type Output = [u8];

    fn index(&self, _: RangeFull) -> &Self::Output {
        &self[0..self.size]
    }
}

impl IndexMut<RangeFull> for Dma<u8> {
    fn index_mut(&mut self, _: RangeFull) -> &mut Self::Output {
        let len = self.size;
        &mut self[0..len]
    }
}

const MAP_HUGE_2MB: i32 = 0x5400_0000; // 21 << 26

impl<T> Dma<T> {
    /// Allocates DMA Memory on a huge page
    // TODO: vfio support?
    pub fn allocate(size: usize) -> Result<Dma<T>, Box<dyn Error>> {
        let size = if size % HUGE_PAGE_SIZE != 0 {
            ((size >> HUGE_PAGE_BITS) + 1) << HUGE_PAGE_BITS
        } else {
            size
        };

        if get_vfio_container().is_some() {
            Self::allocate_vfio(size)
        } else {
            Self::allocate_direct(size)
        }
    }

    fn allocate_vfio(size: usize) -> Result<Dma<T>, Box<dyn Error>> {
        // println!("allocating dma memory via VFIO");

        let ptr = if IOVA_WIDTH < X86_VA_WIDTH {
            // println!("IOVA_WIDTH < X86_VA_WIDTH");
            // To support IOMMUs capable of 39 bit wide IOVAs only, we use
            // 32 bit addresses. Since mmap() ignores libc::MAP_32BIT when
            // using libc::MAP_HUGETLB, we create a 32 bit address with the
            // right alignment (huge page size, e.g. 2 MB) on our own.

            // first allocate memory of size (needed size + 1 huge page) to
            // get a mapping containing the huge page size aligned address
            let addr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    size + HUGE_PAGE_SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_32BIT,
                    -1,
                    0,
                )
            };

            // calculate the huge page size aligned address by rounding up
            let aligned_addr = ((addr as isize + HUGE_PAGE_SIZE as isize - 1)
                & -(HUGE_PAGE_SIZE as isize)) as *mut libc::c_void;

            let free_chunk_size = aligned_addr as usize - addr as usize;

            // free unneeded pages (i.e. all chunks of the additionally mapped huge page)
            unsafe {
                libc::munmap(addr, free_chunk_size);
                libc::munmap(aligned_addr.add(size), HUGE_PAGE_SIZE - free_chunk_size);
            }

            // finally map huge pages at the huge page size aligned 32 bit address
            unsafe {
                libc::mmap(
                    aligned_addr as *mut libc::c_void,
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED
                        | libc::MAP_ANONYMOUS
                        | libc::MAP_HUGETLB
                        | MAP_HUGE_2MB
                        | libc::MAP_FIXED,
                    -1,
                    0,
                )
            }
        } else {
            println!("IOVA_WIDTH >= X86_VA_WIDTH");
            unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB | MAP_HUGE_2MB,
                    -1,
                    0,
                )
            }
        };

        // This is the main IOMMU work: IOMMU DMA MAP the memory...
        if ptr == libc::MAP_FAILED {
            Err(format!(
                "failed to memory map DMA-memory. Errno: {}",
                std::io::Error::last_os_error()
            )
            .into())
        } else {
            let iova = Vfio::map_dma(ptr as usize, size)?;

            let memory = Dma {
                virt: ptr as *mut T,
                phys: iova,
                size,
            };

            Ok(memory)
        }
    }

    fn allocate_direct(size: usize) -> Result<Dma<T>, Box<dyn Error>> {
        let id = HUGEPAGE_ID.fetch_add(1, Ordering::SeqCst);
        let path = format!("/mnt/huge/nvme-{}-{}", process::id(), id);

        match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path.clone())
        {
            Ok(f) => {
                let ptr = unsafe {
                    libc::mmap(
                        ptr::null_mut(),
                        size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED | libc::MAP_HUGETLB,
                        // libc::MAP_SHARED,
                        f.as_raw_fd(),
                        0,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    Err("failed to mmap huge page - are huge pages enabled and free?".into())
                } else if unsafe { libc::mlock(ptr, size) } == 0 {
                    let memory = Dma {
                        // virt: NonNull::new(ptr as *mut T).expect("oops"),
                        virt: ptr as *mut T,
                        phys: virt_to_phys(ptr as usize)?,
                        size,
                    };
                    Ok(memory)
                } else {
                    Err("failed to memory lock huge page".into())
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => Err(Box::new(io::Error::new(
                e.kind(),
                format!(
                    "huge page {} could not be created - huge pages enabled?",
                    path
                ),
            ))),
            Err(e) => Err(Box::new(e)),
        }
    }
}

/// Translates a virtual address to its physical counterpart
pub(crate) fn virt_to_phys(addr: usize) -> Result<usize, Box<dyn Error>> {
    let pagesize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;

    let mut file = fs::OpenOptions::new()
        .read(true)
        .open("/proc/self/pagemap")?;

    file.seek(io::SeekFrom::Start(
        (addr / pagesize * mem::size_of::<usize>()) as u64,
    ))?;

    let mut buffer = [0; mem::size_of::<usize>()];
    file.read_exact(&mut buffer)?;

    let phys = unsafe { mem::transmute::<[u8; mem::size_of::<usize>()], usize>(buffer) };
    Ok((phys & 0x007F_FFFF_FFFF_FFFF) * pagesize + addr % pagesize)
}

#[allow(unused)]
pub fn vfio_enabled() -> bool {
    unsafe { VFIO_CONTAINER_FILE_DESCRIPTOR.is_some() }
}

pub fn get_vfio_container() -> Option<RawFd> {
    unsafe { VFIO_CONTAINER_FILE_DESCRIPTOR }
}

pub fn set_vfio_container(cfd: RawFd) {
    unsafe { VFIO_CONTAINER_FILE_DESCRIPTOR = Some(cfd) }
}
