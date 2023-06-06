#![feature(iter_advance_by)]

use core::{ffi, panic};
use ctru::error::ResultCode;
use ctru_sys::{getThreadCommandBuffer, Handle};
use std::{
    arch::asm,
    ffi::{c_void, CString},
    mem::{size_of, zeroed, MaybeUninit},
};

// TODO: Make this a bitfield struct
#[derive(Debug, Clone, Copy)]
pub struct IPCHeader {
    command: u16,
    normal_params: usize,
    translate_params: usize,
}

impl From<u32> for IPCHeader {
    fn from(header: u32) -> Self {
        let command: u16 = ((header >> 16) & 0xffff).try_into().unwrap();
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
        handles: &'a [Handle],
    },
    Static {
        static_index: usize,
        buffer: &'a [u8],
    },
    Mapped {
        permissions: TranslationPermission,
        buffer: &'a mut [u8],
    },
    Read {
        buffer: &'a [u8],
    },
    Write {
        buffer: *mut c_void,
    },
}

impl From<&Translation<'_>> for Vec<u32> {
    fn from(value: &Translation) -> Self {
        let mut v = vec![];
        match value {
            Translation::Handles {
                closed_for_caller,
                replace_with_process_id,
                handles,
            } => {
                let mut header = ((handles.len() - 1) as u32) << 26;
                if *closed_for_caller {
                    header |= 0x10;
                }

                if *replace_with_process_id {
                    header |= 0x20;
                }

                v.push(header);
                v.extend_from_slice(handles);
            }
            Translation::Static {
                static_index,
                buffer,
            } => {
                let header =
                    0x00000002 | ((buffer.len() as u32) << 14) | (*static_index as u32) << 10;
                v.push(header);
                v.push(buffer.as_ptr() as u32)
            }
            Translation::Mapped {
                permissions,
                buffer,
            } => {
                let header: u32 =
                    0x00000008 | (*permissions as u32) << 1 | (buffer.len() as u32) << 4;
                v.push(header);
                todo!("Convert pointer back into mutable reference");
                v.push((*buffer as *mut [u8] as *mut c_void) as u32);
            }
            Translation::Read { buffer } => todo!(),
            Translation::Write { buffer } => todo!(),
        }
        v
    }
}

struct TranslateParams<'a>(Vec<Translation<'a>>);

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

impl<'a> TranslateParams<'a> {
    pub fn new() -> Self {
        TranslateParams(vec![])
    }

    pub fn add_handles(
        &mut self,
        closed_for_caller: bool,
        replace_with_process_id: bool,
        handles: &'a [Handle],
    ) -> &mut Self {
        self.0.push(Translation::Handles {
            closed_for_caller,
            replace_with_process_id,
            handles,
        });
        self
    }

    fn add_static_buffer(
        &mut self,
        destination_buffer_index: usize,
        buffer: &'a [u8],
    ) -> &mut Self {
        self.0.push(Translation::Static {
            static_index: destination_buffer_index,
            buffer,
        });
        self
    }

    fn add_read_buffer(&mut self, buffer: &'a [u8]) -> &mut Self {
        self.0.push(Translation::Read { buffer });
        self
    }

    fn add_write_buffer(&mut self, buffer: &'a mut [u8]) -> &mut Self {
        self.0.push(Translation::Write {
            buffer: buffer as *mut _ as _,
        });
        self
    }

    fn add_mapped_buffer(
        &mut self,
        permissions: TranslationPermission,
        buffer: &'a mut [u8],
    ) -> &mut Self {
        self.0.push(Translation::Mapped {
            permissions,
            buffer,
        });
        self
    }

    fn finish(&self) -> Vec<u32> {
        self.0
            .iter()
            .map(|translation| Into::<Vec<u32>>::into(translation))
            .flatten()
            .collect()
    }

    fn parse(params: &'a [u32]) -> TranslateParams<'a> {
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
                        handles: &params[(i + 1)..(i + 1 + num)],
                    });

                    iter.advance_by(num).unwrap();
                }
                1 => {
                    let static_buffer_id = ((header >> 10) & 0xf) as usize;

                    // SAFETY: static_buffer_id is always 0-15
                    // SAFETY: We are parsing translation params from a returned IPC call so the referenced
                    //         static buffer must have been initialized from that call
                    let static_buffer = unsafe { get_static_buffer(static_buffer_id) };

                    let data = static_buffer.buffer;
                    let len = (static_buffer.descriptor >> 14) as usize;
                    let buffer = unsafe { std::slice::from_raw_parts_mut(data as *mut u8, len) };

                    v.push(Translation::Static {
                        static_index: static_buffer_id,
                        buffer,
                    })
                }
                5 | 6 | 7 => {
                    let perms: TranslationPermission = (typ & 0x3).into();
                    let size = (*header >> 4) as usize;

                    let (_, ptr) = iter.next().unwrap();
                }
                _ => unreachable!(),
            }
        }
        TranslateParams(v)
    }
}

unsafe fn send_cmd<T, R>(
    handle: Handle,
    command_id: u16,
    obj: T,
    translate: TranslateParams,
) -> Result<(R, TranslateParams), ctru_sys::Result> {
    todo!()
}

const fn bytes_to_words(bytes: usize) -> usize {
    type WORD = u32;
    (bytes + size_of::<WORD>() - 1) / size_of::<WORD>()
}

pub const MAX_IPC_ARGS: usize = 64;

// TODO: Parse pages from the 3dbrew wiki for T, R
// Most pages have the same structure and some even have expressions for
// calculating the correct translation headers

/// Simple wrapper to send IPC commands that do not involve buffer/handle translation.
///
/// Panics if the sizes of types T or R are > [`MAX_IPC_ARGS`] `* size_of::<u32>()`.
///
/// # Safety
///
/// The caller must ensure that a valid object of type R is
/// stored at ThreadCommandBuffer + 0x02 upon success of the command.
pub unsafe fn send_struct<T, R>(
    handle: Handle,
    command_id: u16,
    obj: T,
) -> Result<R, ctru_sys::Result> {
    assert!(bytes_to_words(size_of::<T>()) <= MAX_IPC_ARGS);
    assert!(bytes_to_words(size_of::<R>()) <= MAX_IPC_ARGS);
    let ipc = getThreadCommandBuffer();

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
        bytes_to_words(size_of::<R>() + size_of::<ctru_sys::Result>()) as usize
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
struct StaticBufferPair {
    descriptor: u32,
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
    let base = ctru_sys::getThreadStaticBuffer() as *mut StaticBufferPair;

    // Zero out the static buffer pair when reading so that future calls won't accidentally still have a pointer
    base.add(i).replace(zeroed())
}

/// Sets up a static buffer desciptor and the corresponding pointer for use in a subsequent IPC call
///
/// # Safety
///
/// The pointer must be valid when an IPC call that references the i-th buffer is made
unsafe fn set_static_buffer(i: usize, buffer_descriptor: StaticBufferPair) {
    assert!(i <= 16);
    let base = ctru_sys::getThreadStaticBuffer() as *mut StaticBufferPair;
    base.add(i).write(buffer_descriptor);
}

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

struct HTTPResponse {
    code: u32,
    body: String,
}

enum HTTPMethod {
    GET,
    POST,
    PUT,
    PATCH,
    DELETE,
}

fn IPC_HTTP_SEND(url: &str, method: HTTPMethod, body: &mut str) -> ctru::Result<HTTPResponse> {
    #[repr(C)]
    struct Params {
        method: u32,
        size: usize,
    }

    #[repr(C)]
    struct ReturnParams {
        status: u32,
    }

    let params = Params {
        method: method as u32,
        size: body.len(),
    };

    const BUFFER_LENGTH: usize = 4096;
    let mut output = vec![0u8; BUFFER_LENGTH];

    let url = CString::new(url).unwrap();

    let mut translated = TranslateParams::new();
    translated
        // Write the url to static buffer 0 in the server
        .add_static_buffer(0, url.as_bytes_with_nul())
        .add_read_buffer(body.as_bytes())
        .add_write_buffer(&mut output);

    //let ref1 = &output;

    let handle = 0;
    let (return_params, _translate_params): (ReturnParams, TranslateParams) =
        unsafe { send_cmd(handle, 0x420, params, translated) }?;

    //println!("{:?}", ref1);

    // TODO: Validate that all buffers were returned in translate_params

    let body = String::from_utf8(output).unwrap();

    Ok(HTTPResponse {
        code: return_params.status,
        body,
    })
}
