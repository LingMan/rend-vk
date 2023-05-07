use ash::vk;
use std::cell::RefCell;
use std::clone::Clone;
use std::marker::Copy;
use std::os::raw::c_void;
use std::rc::Rc;

#[derive(Clone)]
pub struct DeviceAllocator {
    inner: Rc<RefCell<InnerDeviceAllocator>>,
    pub buffer: DeviceBuffer,
}

#[derive(Copy, Clone)]
pub struct DeviceSlice {
    pub size: u64,
    pub offset: u64,
    pub alignment: u64,
    pub addr: *mut c_void,
}

impl DeviceAllocator {
    pub fn new_general(
        instance: &ash::Instance,
        physical_device: &vk::PhysicalDevice,
        device: &ash::Device,
        size: u64,
    ) -> Self {
        Self::new(instance, physical_device, device, size, BufferKind::GENERAL)
    }

    pub fn new_descriptor(
        instance: &ash::Instance,
        physical_device: &vk::PhysicalDevice,
        device: &ash::Device,
        size: u64,
    ) -> Self {
        Self::new(
            instance,
            physical_device,
            device,
            size,
            BufferKind::DESCRIPTOR,
        )
    }

    pub fn new(
        instance: &ash::Instance,
        physical_device: &vk::PhysicalDevice,
        device: &ash::Device,
        size: u64,
        kind: BufferKind,
    ) -> Self {
        let inner = InnerDeviceAllocator::new(instance, physical_device, device, size, kind);
        let buffer = inner.buffer.clone();
        let refc = Rc::new(RefCell::new(inner));
        Self {
            buffer,
            inner: refc,
        }
    }

    pub fn alloc(&self, size: u64) -> Option<DeviceSlice> {
        self.inner.borrow_mut().alloc(size)
    }

    pub fn free(&self, slice: DeviceSlice) {
        self.inner.borrow_mut().free(slice)
    }

    pub fn destroy(&self, device: &ash::Device) {
        self.inner.borrow().destroy(device)
    }

    pub fn available(&self) -> u64 {
        self.inner.borrow().available()
    }
}

#[derive(Copy, Clone, PartialEq, strum_macros::Display)]
pub enum BufferKind {
    GENERAL,
    DESCRIPTOR,
}

impl BufferKind {
    fn to_vk_usage_flags(&self) -> vk::BufferUsageFlags {
        use vk::BufferUsageFlags as Buf;
        match self {
            BufferKind::GENERAL => {
                Buf::SHADER_DEVICE_ADDRESS
                    | Buf::VERTEX_BUFFER
                    | Buf::INDEX_BUFFER
                    | Buf::STORAGE_BUFFER
                    | Buf::UNIFORM_BUFFER
                    | Buf::TRANSFER_SRC
                    | Buf::TRANSFER_DST
            }
            BufferKind::DESCRIPTOR => {
                Buf::SHADER_DEVICE_ADDRESS
                    | Buf::RESOURCE_DESCRIPTOR_BUFFER_EXT
                    | Buf::SAMPLER_DESCRIPTOR_BUFFER_EXT
            }
        }
    }
}

#[derive(Copy, Clone)]
struct Range {
    start: u64,
    end: u64,
}

impl Range {
    fn size(&self) -> u64 {
        self.end - self.start
    }
}

struct InnerDeviceAllocator {
    buffer: DeviceBuffer,
    ranges: Vec<Range>,
}

#[derive(Clone)]
pub struct DeviceBuffer {
    pub size: u64,
    pub alignment: u64,
    pub device_addr: u64,
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    pub addr: *mut c_void,
    pub type_index: u32,
    pub kind: BufferKind,
}

impl DeviceBuffer {
    // Max alignment a buffer of any type can have
    const MAX_ALIGNMENT: u64 = 256;

    pub fn new(
        instance: &ash::Instance,
        physical_device: &vk::PhysicalDevice,
        device: &ash::Device,
        size: u64,
        kind: BufferKind,
    ) -> Self {
        use vk::MemoryPropertyFlags as Mpf;
        let usage_flags = kind.to_vk_usage_flags();
        let mem_flags = Mpf::DEVICE_LOCAL | Mpf::HOST_VISIBLE | Mpf::HOST_COHERENT;
        let buffer_info = vk::BufferCreateInfo {
            size: Self::next_size(size, Self::MAX_ALIGNMENT),
            usage: usage_flags,
            sharing_mode: vk::SharingMode::EXCLUSIVE,
            ..Default::default()
        };
        let buffer: vk::Buffer;
        let mem_reqs: vk::MemoryRequirements;
        unsafe {
            buffer = device.create_buffer(&buffer_info, None).unwrap();
            mem_reqs = device.get_buffer_memory_requirements(buffer);
        }
        let alignment = if BufferKind::DESCRIPTOR == kind {
            /*
             * Descriptor offset alignment may be wider than the actual memory
             * alignment, defensively use the bigger of the two.
             */
            std::cmp::max(
                mem_reqs.alignment,
                Self::get_descriptor_offset_alignment(instance, physical_device),
            )
        } else {
            mem_reqs.alignment
        };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(*physical_device) };

        let memi = Self::find_memorytype_index(&mem_reqs, &mem_props, mem_flags)
            .expect("Unable to find suitable memorytype for the buffer");
        let mut mem_flags = vk::MemoryAllocateFlagsInfo {
            flags: vk::MemoryAllocateFlags::DEVICE_ADDRESS,
            ..Default::default()
        };
        let mem_info = vk::MemoryAllocateInfo::builder()
            .allocation_size(mem_reqs.size)
            .memory_type_index(memi)
            .push_next(&mut mem_flags)
            .build();
        let device_addr_info = vk::BufferDeviceAddressInfo {
            buffer,
            ..Default::default()
        };
        let mem: vk::DeviceMemory;
        let addr: *mut c_void;
        let device_addr: u64;
        unsafe {
            mem = device.allocate_memory(&mem_info, None).unwrap();
            addr = device
                .map_memory(mem, 0, mem_reqs.size, vk::MemoryMapFlags::empty())
                .unwrap();
            device.bind_buffer_memory(buffer, mem, 0).unwrap();
            device_addr = device.get_buffer_device_address(&device_addr_info);
        }
        return Self {
            type_index: memi,
            buffer,
            addr,
            kind,
            device_addr,
            alignment,
            memory: mem,
            size: mem_info.allocation_size,
        };
    }

    fn get_descriptor_offset_alignment(
        instance: &ash::Instance,
        physical_device: &vk::PhysicalDevice,
    ) -> u64 {
        let mut props = vk::PhysicalDeviceDescriptorBufferPropertiesEXT {
            ..Default::default()
        };
        let mut device_props = vk::PhysicalDeviceProperties2::builder()
            .push_next(&mut props)
            .build();
        unsafe { instance.get_physical_device_properties2(*physical_device, &mut device_props) };

        props.descriptor_buffer_offset_alignment
    }

    fn next_size(base: u64, mul: u64) -> u64 {
        let mask = -(mul as i64) as u64;
        (base + (mul - 1)) & mask
    }

    fn find_memorytype_index(
        memory_req: &vk::MemoryRequirements,
        memory_prop: &vk::PhysicalDeviceMemoryProperties,
        flags: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        memory_prop.memory_types[..memory_prop.memory_type_count as _]
            .iter()
            .enumerate()
            .find(|(index, memory_type)| {
                (1 << index) & memory_req.memory_type_bits != 0
                    && memory_type.property_flags & flags == flags
            })
            .map(|(index, _memory_type)| index as _)
    }
}

impl InnerDeviceAllocator {
    fn new(
        instance: &ash::Instance,
        physical_device: &vk::PhysicalDevice,
        device: &ash::Device,
        size: u64,
        kind: BufferKind,
    ) -> Self {
        let buffer = DeviceBuffer::new(instance, physical_device, device, size, kind);
        Self::wrap(buffer)
    }

    fn wrap(buffer: DeviceBuffer) -> Self {
        let ranges = vec![Range {
            start: 0,
            end: buffer.size,
        }];
        return Self { buffer, ranges };
    }

    fn alloc(&mut self, size: u64) -> Option<DeviceSlice> {
        let size = DeviceBuffer::next_size(size, self.buffer.alignment);
        let ranges = &mut self.ranges;
        for i in 0..ranges.len() {
            let range = &ranges[i];
            let range_size = range.size();
            if range_size < size {
                continue;
            }
            let old_start = range.start;
            let new_start = old_start + size;
            if new_start == range.end {
                // Took the range
                ranges.remove(i);
            }
            let range = &mut ranges[i];
            range.start = new_start;
            let mut addr = self.buffer.addr;
            let offset;
            unsafe {
                addr = addr.offset(old_start as isize);
                offset = addr.offset_from(self.buffer.addr) as u64;
            }
            return Some(DeviceSlice {
                addr,
                size,
                offset,
                alignment: self.buffer.alignment,
            });
        }
        return None;
    }

    fn free(&mut self, slice: DeviceSlice) {
        // | | | | | |
        let slice_start = unsafe { slice.addr.offset(-(self.buffer.addr as isize)) as u64 };
        let slice_end = slice_start + slice.size;
        let mut idx = 0;
        for i in 0..self.ranges.len() {
            idx = i;
            let range = self.ranges[i];
            if range.start <= slice_start {
                continue;
            }
            if range.start == slice_end {
                let mut new_start = slice_start;
                if i > 0 {
                    let prev_range = self.ranges[i - 1];
                    if prev_range.end == slice_start {
                        //  . <- remove
                        // |f|f|o|o|
                        new_start = prev_range.start;
                        idx = i - 1;
                        self.ranges.remove(idx);
                    }
                }
                //  . <- extend backwards
                // |f|o|o|
                let range = &mut self.ranges[idx];
                range.start = new_start;
                return;
            }
            if i != 0 {
                let prev_range = &mut self.ranges[i - 1];
                if prev_range.end == slice_start {
                    //  . <- extend forwards
                    // |f|o|o|
                    prev_range.end = slice_end;
                    return;
                }
                //    . <- insert
                // |o|f|o|
            }
            //  . <- insert
            // |f|o|o|
            break;
        }
        self.ranges.insert(
            idx,
            Range {
                start: slice_start,
                end: slice_end,
            },
        );
    }

    fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.destroy_buffer(self.buffer.buffer, None);
            device.free_memory(self.buffer.memory, None);
        }
    }

    fn available(&self) -> u64 {
        self.ranges.iter().map(|r| r.size()).sum()
    }
}
