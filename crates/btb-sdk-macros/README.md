# btb-sdk-macros

Proc macros behind `btb_sdk::btb`. Each function macro keeps the function and adds
`#[no_mangle] pub extern "C" fn btb_<kind>_<name>(ptr: *const u8, len: usize)`, which decodes the input as the tuple of the arguments, calls the function and passes the encoded result to `return_data`. `#[btb::table]` implements `Encode`, `Decode` (fields in order) and `btb_sdk::table::Table`. Use through `btb-sdk`, not directly.
