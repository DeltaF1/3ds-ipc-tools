#![feature(iter_advance_by)]

use core::{ffi, panic};
use ctru::error::ResultCode;
use ctru_sys::Handle;
use std::{
    arch::asm,
    ffi::{c_void, CString},
    marker::PhantomData,
    mem::{size_of, size_of_val, zeroed},
};

// TODO: Delete this in favour of implementing this in ctru_sys
mod tls {
    use core::ffi;
    #[inline]
    #[allow(non_snake_case)]
    pub unsafe fn getThreadLocalStorage() -> *mut ffi::c_void {
        let ret;
        core::arch::asm!(
        "mrc p15, 0, {ptr}, c13, c0, 3",
        ptr = out(reg) ret);
        ret
    }

    #[inline]
    #[allow(non_snake_case)]
    pub unsafe fn getThreadCommandBuffer() -> *mut u32 {
        (getThreadLocalStorage() as *mut u8).add(0x80) as *mut u32
    }

    #[inline]
    #[allow(non_snake_case)]
    pub unsafe fn getThreadStaticBuffer() -> *mut u32 {
        (getThreadLocalStorage() as *mut u8).add(0x180) as *mut u32
    }
}

// TODO: Make this a bitfield struct
#[derive(Debug, Clone, Copy)]
pub struct IPCHeader {
    command: u16,
    normal_params: usize,
    translate_params: usize,
}

impl IPCHeader {
    pub fn new(command: u16, normal_params: usize, translate_params: usize) -> Self {
        assert!(normal_params + translate_params <= MAX_IPC_ARGS);
        IPCHeader {
            command,
            normal_params,
            translate_params,
        }
    }
}

impl From<u32> for IPCHeader {
    fn from(header: u32) -> Self {
        let command: u16 = ((header >> 16) & 0xffff) as u16;
        let normal_params = ((header >> 6) & 0x3f) as usize;
        let translate_params = (header & 0x3f) as usize;

        IPCHeader {
            command,
            normal_params,
            translate_params,
        }
    }
}

impl From<IPCHeader> for u32 {
    fn from(header: IPCHeader) -> Self {
        (header.command as u32) << 16
            | ((header.normal_params as u32) & 0x3f) << 6
            | (header.translate_params as u32) & 0x3f
    }
}

enum Translation<'a> {
    Handles {
        closed_for_caller: bool,
        replace_with_process_id: bool,
        handles: Vec<Handle>,
    },
    Static {
        static_index: usize,
        ptr: *const c_void,
        len: usize,
        _marker: PhantomData<&'a ()>,
    },
    Read {
        ptr: *const c_void,
        len: usize,
        _marker: PhantomData<&'a ()>,
    },
    Write {
        ptr: *mut c_void,
        len: usize,
        _marker: PhantomData<&'a mut ()>,
    },
    ReadWrite {
        ptr: *mut c_void,
        len: usize,
        _marker: PhantomData<&'a mut ()>,
    },
}

/// Serialize translation param to its cmdbuf representation
impl From<&Translation<'_>> for Vec<u32> {
    fn from(value: &Translation) -> Self {
        let mut v = vec![];
        match value {
            Translation::Handles {
                closed_for_caller,
                replace_with_process_id,
                handles,
            } => {
                let header =
                    handle_descriptor(handles.len(), *closed_for_caller, *replace_with_process_id);
                v.push(header);
                v.extend_from_slice(handles);
            }
            Translation::Static {
                static_index,
                ptr,
                len,
                _marker,
            } => {
                let header = static_buffer_descriptor(*static_index, *len);
                v.push(header);
                v.push(*ptr as u32)
            }
            Translation::Read { ptr, len, _marker } => {
                let header = read_buffer_descriptor(*len);
                v.push(header);
                v.push(*ptr as u32);
            }
            Translation::Write { ptr, len, _marker } => {
                let header = write_buffer_descriptor(*len);
                v.push(header);
                v.push(*ptr as u32);
            }
            Translation::ReadWrite { ptr, len, _marker } => {
                let header = read_write_buffer_descriptor(*len);
                v.push(header);
                v.push(*ptr as u32);
            }
        }
        v
    }
}

const STATIC_BUFFER_MAGIC: u32 = 0x00000002;
const READ_BUFFER_MAGIC: u32 = 0x0000000A;
const WRITE_BUFFER_MAGIC: u32 = 0x0000000C;
const READ_WRITE_BUFFER_MAGIC: u32 = 0x0000000E;

pub const fn static_buffer_descriptor(static_index: usize, len: usize) -> u32 {
    STATIC_BUFFER_MAGIC | ((len as u32) << 14) | (static_index as u32) << 10
}

pub const fn read_buffer_descriptor(len: usize) -> u32 {
    READ_BUFFER_MAGIC | ((len as u32) << 4)
}

pub const fn write_buffer_descriptor(len: usize) -> u32 {
    WRITE_BUFFER_MAGIC | ((len as u32) << 4)
}

pub const fn read_write_buffer_descriptor(len: usize) -> u32 {
    READ_WRITE_BUFFER_MAGIC | ((len as u32) << 4)
}

pub const fn handle_descriptor(
    len: usize,
    closed_for_caller: bool,
    replace_with_process_id: bool,
) -> u32 {
    let mut header = (len as u32) << 26;
    if closed_for_caller {
        header |= 0x10;
    }

    if replace_with_process_id {
        header |= 0x20;
    }

    header
}

/// Struct to construct translated paramters for an IPC call.
#[derive(Default)]
pub struct TranslateParams<'a>(Vec<Translation<'a>>);

#[repr(u32)]
#[derive(Clone, Copy)]
enum TranslationPermission {
    Read = 1,
    Write = 2,
    ReadWrite = 3,
}

impl From<u32> for TranslationPermission {
    fn from(value: u32) -> Self {
        match value {
            1 => Self::Read,
            2 => Self::Write,
            3 => Self::ReadWrite,
            _ => panic!("Invalid TranslationPermission {value} for Buffer mapping translation"),
        }
    }
}

// https://play.rust-lang.org/?version=stable&mode=debug&edition=2021&gist=8160b63e6c20035e1540603f657b00a2
// the PhantomData seems to be enough to keep borrows until this struct is dropped
impl<'a> TranslateParams<'a> {
    pub fn new() -> Self {
        TranslateParams(vec![])
    }

    // TODO: Vec<Handle> has a lot of overhead for a small amount of handles, is there a better
    // way to handle the lifetimes when parsing? Or maybe just use a SmallVec
    // Alternatively, write a whole new kind of struct to hold returned translate params
    /// Send handles to the destination process
    pub fn add_handles(
        &mut self,
        closed_for_caller: bool, // TODO: Replace this with a HandleOptions enum
        replace_with_process_id: bool,
        handles: Vec<Handle>,
    ) -> &mut Self {
        self.0.push(Translation::Handles {
            closed_for_caller,
            replace_with_process_id,
            handles,
        });
        self
    }

    // TODO: Rename to "send_static_buffer" or something to make it clear it's not for receiving
    /// Copy a buffer to the destination process's i-th static buffer.
    ///
    /// Immutably borrows `buffer` for the duration of the IPC call
    pub fn add_static_buffer<T: ?Sized>(
        &mut self,
        destination_buffer_index: usize,
        buffer: &'a T,
    ) -> &mut Self {
        self.0.push(Translation::Static {
            static_index: destination_buffer_index,
            ptr: buffer as *const T as _,
            len: size_of_val(buffer),
            _marker: PhantomData,
        });
        self
    }

    /// Map a buffer into the destination's memory space with read-only permissions
    ///
    /// Immutably borrows `buffer` for the duration of the IPC call
    pub fn add_read_buffer<T: ?Sized>(&mut self, buffer: &'a T) -> &mut Self {
        self.0.push(Translation::Read {
            ptr: buffer as *const T as _,
            len: size_of_val(buffer),
            _marker: PhantomData,
        });
        self
    }

    /// Map a buffer into the destination's memory space with write-only permissions
    ///
    /// Mutably borrows `buffer` for the duration of the IPC call
    pub fn add_write_buffer<T: ?Sized>(&mut self, buffer: &'a mut T) -> &mut Self {
        self.0.push(Translation::Write {
            ptr: buffer as *mut _ as _,
            len: size_of_val(buffer),
            _marker: PhantomData,
        });
        self
    }

    /// Map a buffer into the destination's memory space with read & write permissions
    ///
    /// Mutably borrows `buffer` for the duration of the IPC call
    pub fn add_read_write_buffer<T: ?Sized>(&mut self, buffer: &'a mut T) -> &mut Self {
        self.0.push(Translation::ReadWrite {
            ptr: buffer as *mut _ as _,
            len: size_of_val(buffer),
            _marker: PhantomData,
        });
        self
    }

    // TODO: self vs &self? Should this be unsafe?
    /// Serialize into the format expected by [`svcSendSyncRequest`]
    ///
    /// # Safety
    ///
    /// Calling this can create dangling pointers if the original [`TranslateParams`]
    /// is dropped before the Vec is used, since that will free up the lifetimes of the
    /// references that were used to create it.
    pub fn finish(&self) -> Vec<u32> {
        self.0.iter().flat_map(Into::<Vec<u32>>::into).collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Parse the translate params returned from an IPC call
    ///
    /// The main purpose of this is to retrieve any [`Handle`]s that have been sent back
    pub fn parse_returned_params(params: &[u32]) -> TranslateParams<'_> {
        assert!(params.len() <= MAX_IPC_ARGS);
        let mut v = vec![];
        let mut iter = params.iter().enumerate();
        while let Some((i, header)) = iter.next() {
            let typ = (*header & 0xe) >> 1;
            match typ {
                0 => {
                    let num = ((header >> 26) + 1) as usize;
                    let closed_for_caller = header & 0x10 > 0;
                    let replace_with_process_id = header & 0x20 > 0;

                    v.push(Translation::Handles {
                        closed_for_caller,
                        replace_with_process_id,
                        handles: Vec::from(&params[(i + 1)..(i + 1 + num)]),
                    });

                    iter.advance_by(num).unwrap();
                }
                1 => {
                    unimplemented!("Static buffer descriptor should not be de-serialized")
                    /*
                    let static_buffer_id = ((header >> 10) & 0xf) as usize;

                    let static_buffer = unsafe { get_static_buffer(static_buffer_id) };

                    let data = static_buffer.buffer;
                    let len = (static_buffer.descriptor >> 14) as usize;

                    v.push(Translation::Static {
                        static_index: static_buffer_id,
                        ptr: data,
                        len,
                        _marker: PhantomData,
                    })
                    */
                }
                5 | 6 | 7 => {
                    let perms: TranslationPermission = (typ & 0x3).into();
                    let size = (*header >> 4) as usize;

                    let (_, ptr) = iter.next().unwrap();

                    v.push(match perms {
                        TranslationPermission::Read => Translation::Read {
                            ptr: *ptr as *const _,
                            len: size,
                            _marker: PhantomData,
                        },
                        TranslationPermission::Write => Translation::Write {
                            ptr: *ptr as *mut _,
                            len: size,
                            _marker: PhantomData,
                        },
                        TranslationPermission::ReadWrite => todo!(),
                    });
                }
                _ => unreachable!(),
            }
        }
        TranslateParams(v)
    }

    // TODO: function to retrieve handles
}

#[derive(Default)]
pub struct StaticReceiveParams<'a> {
    _marker: PhantomData<&'a ()>,
    buffers: [Option<StaticBufferPair>; 16],
}

impl<'a> StaticReceiveParams<'a> {
    pub fn new() -> Self {
        StaticReceiveParams::default()
    }

    pub fn add_receive_buffer<T>(&mut self, i: usize, r: &'a mut T) {
        debug_assert!(
            self.buffers[i].is_none(),
            "Tried to set two outputs for the same static buffer"
        );

        self.buffers[i] = Some(StaticBufferPair {
            descriptor: static_buffer_descriptor(i, size_of_val(r)),
            buffer: r as *mut _ as _,
        });
    }
}

// TODO: Define safety requirements more rigorously
/// Send an IPC command to a service handle
///
/// # Safety
///
/// In general, to use an IPC command safely you should read the [3dbrew
/// wiki](https://www.3dbrew.org/wiki/) page for that command.
///
/// This is a non-exhaustive list of safety requirements to use this function. This is a foreign call
/// so the receiving code could do anything!
///
/// 1. The size of `T` and the translate parameters (in `u32`s) must match the expected size for
///    the given IPC command (from the wiki page). In particular:
///     ```rs
///     (size_of::<T>() + size_of::<u32>() - 1) / size_of::<u32>()
///     ```
///    must be equal to the expected `normal_params` and
///     ```rs
///     translate_params.len()
///     ```
///    must be equal to the expected `translate_params`.
/// 2. `static_receive_buffers` must contain an entry for each of the static buffers that the IPC
///    command might send data to.
/// 3. In the case of a 0 (non-error) Result, the size of `R` (in `u32`s) must be equal to `normal_params - 1` in
///    the returned IPC response header.
/// 4.  The caller must ensure that a valid object of type R is
///     stored at ThreadCommandBuffer + 0x02 upon success of the command.
pub unsafe fn send_cmd<'a, T, R>(
    handle: Handle,
    command_id: u16, // TODO: take in an IPCHeader instead and assert that the sizes match?
                     //       This would make it easier to ensure that the params match the wiki
    obj: T,
    translate: TranslateParams,
    static_receive_buffers: StaticReceiveParams,
) -> Result<(R, TranslateParams<'a>), ctru_sys::Result> {
    assert!(bytes_to_words(size_of::<T>()) <= MAX_IPC_ARGS);
    assert!(bytes_to_words(size_of::<R>()) <= MAX_IPC_ARGS);

    unsafe {
        for (i, buffer) in static_receive_buffers.buffers.iter().enumerate() {
            if let Some(buf) = buffer {
                // TODO: Make this more efficient by not calling getThreadStaticBuffer multiple times
                set_static_buffer(i, buf.clone());
            }
        }
    }

    let translated_raw = translate.finish();
    assert!(translated_raw.len() <= MAX_IPC_ARGS);

    let header = IPCHeader {
        command: command_id,
        normal_params: bytes_to_words(size_of::<T>()),
        translate_params: translated_raw.len(),
    };

    let ipc_buf = tls::getThreadCommandBuffer();

    ipc_buf.write(header.into());

    // Write normal params at cmdbuf[1:normal_params + 1]
    (ipc_buf.add(1) as *mut T).write(obj);

    // Write translate params at cmdbuf[normal_params + 1:]
    ipc_buf
        .add(header.normal_params + 1)
        .copy_from_nonoverlapping(translated_raw.as_ptr(), translated_raw.len());

    // Syscall
    let res = ctru_sys::svcSendSyncRequest(handle);

    // Delete all the static buffer descriptors after the call is done, even if it failed
    unsafe {
        for (i, buffer) in static_receive_buffers.buffers.iter().enumerate() {
            if buffer.is_some() {
                get_static_buffer(i);
            }
        }
    }

    if res < 0 {
        return Err(res);
    }

    let response_header = *ipc_buf;
    let response_header = IPCHeader::from(response_header);
    //println!("response: {response_header:?}");

    // Safety check to ensure that the kernel gave back a reply with the correct size
    // TODO: On error does it only set normal_params to 1?
    assert_eq!(
        response_header.normal_params,
        bytes_to_words(size_of::<R>() + size_of::<ctru_sys::Result>())
    );

    let res = *ipc_buf.add(1) as i32;
    if res < 0 {
        return Err(res);
    }

    // This part is potentially unsound dependent upon safe code >.<
    // E.g. if the return data is not valid for R then the type will be invalid
    let retval = ipc_buf.add(2) as *const R;

    // SAFETY: This ref is dropped at the end of this method
    let translate_buf = std::slice::from_raw_parts(
        ipc_buf.add(response_header.normal_params + 1) as *const u32,
        response_header.translate_params,
    );

    let translate_returned = TranslateParams::parse_returned_params(translate_buf);

    Ok((retval.read(), translate_returned))
}

const fn bytes_to_words(bytes: usize) -> usize {
    type WORD = u32;
    (bytes + size_of::<WORD>() - 1) / size_of::<WORD>()
}

// TODO: What is the actual limit here? It might be 63 because the header takes up 1 
pub const MAX_IPC_ARGS: usize = 64;

// TODO: Parse pages from the 3dbrew wiki for T, R
// Most pages have the same structure and some even have expressions for
// calculating the correct translation headers

/// Simple wrapper to send IPC commands that do not involve buffer/handle translation.
///
/// Panics if the sizes of types T or R are > [`MAX_IPC_ARGS`] `* size_of::<u32>()`.
///
/// # Safety
/// In general, to use an IPC command safely you should read the [3dbrew
/// wiki](https://www.3dbrew.org/wiki/) page for that command.
///
/// This is a non-exhaustive list of safety requirements to use this function. This is a foreign call
/// so the receiving code could do anything! See [`send_cmd`] for more details
///
/// 1. The caller must ensure that a valid object of type R is
///    stored at ThreadCommandBuffer + 0x02 upon success of the command.
pub unsafe fn send_struct<T, R>(
    handle: Handle,
    command_id: u16,
    obj: T,
) -> Result<R, ctru_sys::Result> {
    assert!(bytes_to_words(size_of::<T>()) <= MAX_IPC_ARGS);
    assert!(bytes_to_words(size_of::<R>()) <= MAX_IPC_ARGS);
    let ipc = tls::getThreadCommandBuffer();

    let header = IPCHeader {
        command: command_id,
        normal_params: bytes_to_words(size_of::<T>()),
        translate_params: 0,
    };
    *ipc = <u32>::from(header);
    (ipc.add(1) as *mut T).write(obj);
    let res = ctru_sys::svcSendSyncRequest(handle);
    if res < 0 {
        return Err(res);
    }

    let response_header = *ipc;
    let response_header = IPCHeader::from(response_header);
    //println!("response: {response_header:?}");

    // Safety check to ensure that the kernel gave back a reply with the correct size
    // TODO: On error does it only set normal_params to 1?
    assert_eq!(
        response_header.normal_params,
        bytes_to_words(size_of::<R>() + size_of::<ctru_sys::Result>())
    );

    // SAFETY: u32 is Copy
    let res = *ipc.add(1) as i32;
    if res < 0 {
        return Err(res);
    }

    // This part is potentially unsound dependent upon safe code >.<
    // E.g. if R does not fit the return data then the type will be invalid
    let retval = ipc.add(2) as *const R;

    // FIXME: Should this just return a pointer?
    Ok(retval.read())
}

#[repr(C)]
#[derive(Clone)]
struct StaticBufferPair {
    descriptor: u32,

    // TODO: NotNull<c_void> ?
    // Some IPC calls might use the null as a marker though like in the case of null pointers in https://www.3dbrew.org/wiki/AM:ReadTwlBackupInfo
    buffer: *mut c_void,
}

// TODO: Would be nice if all calls to IPC commands needed a &mut threadCommandBuffer/threadStaticBuffer
/// Retrieve the i-th static buffer
///
/// # Safety
///
/// The specified buffer must have been initialized by a previous IPC call or call to [set_static_buffer]
///
/// The returned pointer is only valid until the next IPC call at which time
/// it may be overwritten by the kernel.
unsafe fn get_static_buffer(i: usize) -> StaticBufferPair {
    assert!(i <= 16);
    let base = tls::getThreadStaticBuffer() as *mut StaticBufferPair;

    // Zero out the static buffer pair when reading so that future calls won't accidentally still have a pointer
    base.add(i).replace(zeroed())
}

/// Sets up a static buffer desciptor and the corresponding pointer for use in a subsequent IPC call
///
/// # Safety
///
/// The pointer must be valid when an IPC call that references the i-th buffer is made
#[inline]
unsafe fn set_static_buffer(i: usize, buffer_descriptor: StaticBufferPair) {
    assert!(i <= 16);
    let base = tls::getThreadStaticBuffer() as *mut StaticBufferPair;
    base.add(i).write(buffer_descriptor);
}

/// Acquire a handle to the named service. Hangs if handle is not available
pub fn get_service_handle(name: &str) -> ctru::Result<Handle> {
    let cstr = CString::new(name).unwrap();
    let mut handle: Handle = 0;
    unsafe {
        ResultCode(ctru_sys::srvInit())?;
        ResultCode(ctru_sys::srvGetServiceHandle(&mut handle, cstr.as_ptr()))?;
        ctru_sys::srvExit();
    }
    Ok(handle)
}

#[repr(i32)]
enum ServiceOp {
    StealClientSession = 0,
    GetName,
}

// TODO: #[cfg(feature = "luma-extensions")]
#[allow(non_snake_case)]
unsafe fn svcControlService(
    op: ServiceOp,
    ptr1: *mut ffi::c_void,
    ptr2: *const ffi::c_void, // FIXME: This is a ptr in one call and a Handle in another
) -> ctru_sys::Result {
    let res;
    unsafe {
        asm!("svc 0xB0", inout("r0") op as i32 => res, in("r1") ptr1, in("r2") ptr2);
    }
    res
}

// TODO: Figure out which Luma version added this
/// Copies a handle for the named service, even if that service has limitations on how many
/// processes may hold a handle at once. This can be used to, for example, access i2c devices
/// directly while they are being used by sysmodules.
///
/// Only available on systems with Luma3DS
///
/// # Safety
///
/// Using the stolen handle could probably cause UB at any point `¯\_(ツ)_/¯`
pub unsafe fn steal_service_handle(name: &str) -> ctru::Result<Handle> {
    let cstr = CString::new(name).unwrap();
    let mut handle: Handle = 0;
    let res = unsafe {
        svcControlService(
            ServiceOp::StealClientSession,
            &mut handle as *mut Handle as *mut c_void,
            cstr.as_ptr() as *const c_void,
        )
    };
    ResultCode(res)?;
    Ok(handle)
}

/// Get the name of a service from its handle
pub fn get_service_name(handle: Handle) -> ctru::Result<String> {
    let mut name = [0 as ffi::c_char; 12];
    let res = unsafe {
        svcControlService(
            ServiceOp::GetName,
            &mut name as *mut _ as *mut _,
            handle as _, // This isn't a ptr but this method can be called with different args so idk how to make it better
        )
    };
    ResultCode(res)?;
    let cstr = ffi::CStr::from_bytes_until_nul(&name).unwrap();
    Ok(String::from(cstr.to_str().unwrap()))
}
