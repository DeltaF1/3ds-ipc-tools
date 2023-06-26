⚠️ Not finished/tested yet, this is just published to get feedback on the design ⚠️
# ipc-tools
library to help make safe IPC wrappers for ctru-rs

Thanks to piepie62, declan, Kartik on the Nintendo Homebrew Discord for answering my questions about IPC semantics.

## How to use

Install devKitPro and https://github.com/rust3ds/cargo-3ds/.

The two main entry points for this library are `send_struct` and `send_cmd`. `send_struct` is a simple wrapper for IPC commands that only have normal params in the request and response. `send_cmd` has more arguments which allow it to express IPC commands with translate params and static buffer-returned values.

For more information run `cargo 3ds doc`.

## Example

1. Find the wiki page for the command you want to wrap. In this example we will be using https://www.3dbrew.org/wiki/AM:ReadTwlBackupInfo.

2. Look at the "Request" table on the wiki page

The first word in the table is the header, whose format is here: https://www.3dbrew.org/wiki/IPC#Message_Structure

|Index Word |Description|
|:----|:----|
|0 |Header code [0x001E00C8]|

From this header we can see that the command id is 0x001E, there are 3 normal params, and there are 8 translate params.

```rs
const COMMAND: u16 = 0x001E;
```

From the above header we know that the next 3 words are "normal" params which means they are passed directly to the destination process without modification.

|Index Word |Description|
|:----|:----|
|1 |Output Info Size (usually 0x20)|
|2 |Banner Size (usually 0x4000)|
|3 |Working Buffer Size|

We can represent these as a simple struct:

```rs
#[repr(C)] // ⚠️ This part is very important to ensure that all these fields are in the correct order without padding in between ⚠️
struct Params {
    output_info_size: usize,
    banner_size: usize,
    working_buffer_size: usize
}
```

The next 8 words are the "translate" params. These parameters are used to transfer larger or more complicated pieces of data between processes. Each paramter consists of a header followed by some number of data words afterwards. To encapsulate these parameters we construct a `TranslateParams` struct.

```rs
let mut translate_params = TranslateParams::new();
```

The first parameter has a header to move one (1) Handle to the destination process. From the wiki section about [Handle translation](https://www.3dbrew.org/wiki/IPC#Handle_Translation) we can see that this header has the "close for caller" bit set.

|Index Word |Description|
|:----|:----|
|4 |0x10 (Magic Word Header, 0x10 = HANDLE_MOVE, we are moving this handle into the IPC server)|
|5 |FSFile Handle|

```rs
// Assuming that we already have a handle to the correct file called `file_handle`
translate_params.add_handles(true, false, vec![file_handle]);
```

The second paramater ends in `0xC` which means it's a write-only [mapped buffer descriptor](https://www.3dbrew.org/wiki/IPC#Buffer_Mapping_Translation). 

|Index Word |Description|
|:----|:----|
|6 |(Output Info Size << 4) \| 0xC|
|7 |TwlBackupInfo Output Pointer. Processing is skipped for this when the pointer is NULL.|

We can add one of these parameters with a mutable reference.

```rs
// Assuming we have a struct TwlBackupInfo
let mut output_info = TwlBackupInfo::new();
translate_params.add_write_buffer(&mut output_info)
```

The next two parameters are similar

|Index Word |Description|
|:----|:----|
|8 |(Banner Size << 4) \| 0xC|
|9 |DSiWare Banner Output Pointer. Processing is skipped for this when the pointer is NULL.|
|10 |(Working Buffer Size << 4) \| 0xC|
|11 |Working Buffer Pointer |

```rs
let mut banner = Banner::new();
translate_params.add_write_buffer(&mut banner);

let mut working_buffer = [0u8; 0x4000];
translate_params.add_write_buffer(&mut working_buffer);
```

3. Finally, look at the "Response" table

The first word of response is again an IPC command header. This has no use to the programmer other than getting the number of return parameters, but the library will take care of that part.

|Index Word |Description|
|:----|:----|
|0 |Header code|

The next word is a Result code. This value will be an error code if the call failed for some reason, or 0 if it succeeded. This is also taken care of by the library.

|Index Word |Description|
|:----|:----|
|1 |Result code|

The words following the result code are the return parameters. Most wiki pages don't say exlicitly which ones are normal params and which are translated, but it's usually easy to deduce from context clues (e.g. we can see the `0xC` descriptor tag and the fact that "pointers" are mentioned). In this case there are no normal parameters and the buffers we mapped earlier are returned to us as translate parameters. The lack of normal parameters can be represented by passing `()` as the return type.

```rs
type Return = ();
```

In the case of mapped buffers there is no need to do anything with this return data so we can move on.

|Index Word |Description|
|:----|:----|
|2 |(Output Info Size << 4) | 0xC|
|3 |TwlBackupInfo Output Pointer.|
|4 |(Banner Size << 4) | 0xC|
|5 |DSiWare Banner Output Pointer.|
|6 |(Working Buffer Size << 4) | 0xC|
|7 |Working Buffer Pointer |

4. Now it's time to put all of this together.

```{no_run, rust}
# // Stubbed types
# struct Banner {} struct TwlBackupInfo {}
# impl Banner {fn new() -> Self { todo!() } } impl TwlBackupInfo { fn new() -> Self { todo!() } }
use std::mem::size_of_val;
use ctru_sys::Handle;
use ipc_tools::HandleOptions;

fn ReadTwlBackupInfo(
    service_handle: Handle,
    file_handle: Handle,
) -> ctru::Result<(TwlBackupInfo, Banner)> {
    const COMMAND: u16 = 0x001E;
    #[repr(C)] // ⚠️ This part is very important to ensure that all these fields are in the correct order without padding in between ⚠️
    struct Params {
        output_info_size: usize,
        banner_size: usize,
        working_buffer_size: usize,
    }

    // Assuming we have some structs already defined for these parameters
    let mut output_info = TwlBackupInfo::new();
    let mut banner = Banner::new();
    let mut working_buffer = [0u8; 0x4000];

    // Construct the normal parameters
    let params = Params {
        output_info_size: size_of_val(&output_info),
        banner_size: size_of_val(&banner),
        working_buffer_size: size_of_val(&working_buffer),
    };

    // Adding translate params can be chained together for convenience
    let mut translate_params = ipc_tools::TranslateParams::new();
    translate_params
        .add_handles(HandleOptions::MoveHandles, vec![file_handle])
        .add_write_buffer(&mut output_info)
        .add_write_buffer(&mut banner)
        .add_write_buffer(&mut working_buffer);

    // In this case there are no parameters being passed back and we don't care about the translate parameters 
    type Return = ();
    let (_normal_return, _translate_return): (Return, ipc_tools::TranslateParams) = unsafe {
        ipc_tools::send_cmd::<Params, Return>(
            service_handle,
            COMMAND,
            params,
            translate_params,
            ipc_tools::StaticReceiveParams::default(),
        )
    }?; // ⚠️ Don't forget to `?` or otherwise handle the Result ⚠️

    Ok((output_info, banner))
}
```
