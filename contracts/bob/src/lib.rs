// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) DUSK NETWORK. All rights reserved.

#![no_std]
#![feature(arbitrary_self_types)]

extern crate alloc;

mod state;
use state::Bob;

#[cfg(target_family = "wasm")]
#[path = ""]
mod wasm {
    use super::*;

    #[no_mangle]
    static mut STATE: Bob = Bob::new();

    #[no_mangle]
    unsafe fn ping(arg_len: u32) -> u32 {
        rusk_abi::wrap_call(arg_len, |()| STATE.ping())
    }
}
