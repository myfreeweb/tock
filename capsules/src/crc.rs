//! CRC driver
//!
//! This capsule provides userspace access to a CRC unit.
//!
//! The `allow` syscall for this driver supports the single
//! `allow_number` zero, which is used to provide a buffer over which
//! to compute a CRC computation.
//!
//! The `subscribe` syscall supports the single `subscribe_number`
//! zero, which is used to provide a callback that will receive the
//! result of a CRC computation.  When the callback is invoked, the
//! first two arguments mean:
//!
//!   * `status`: a return code indicating whether the computation
//!     succeeded.  The status `EBUSY` indicates the unit is already
//!     busy.  The status `ESIZE` indicates the provided buffer is
//!     too large for the unit to handle.
//!
//!   * `result`: when `status == SUCCESS`, the result
//!     of the CRC computation.
//!
//! The `command` syscall supports these `command_number`s:
//!
//!   *   `0`: Returns non-zero to indicate the driver is present
//!
//!   *   `1`: Returns the CRC unit's version value.  This is provided
//!       in order to be complete, but has limited utility as no
//!       consistent semantics are specified.
//!
//!   *   `2`: Requests that a CRC be computed over the buffer
//!       previously provided by `allow`.  If none was provided,
//!       this command will return `EINVAL`.
//!
//!       This command's driver-specific argument indicates what CRC
//!       algorithm to perform, as listed below.  If an invalid
//!       algorithm specifier is provided, this command will return
//!       `EINVAL`.
//!
//!       If a callback was not previously registered with
//!       `subscribe`, this command will return `EINVAL`.
//!
//!       If a computation has already been requested by this
//!       application but the callback has not yet been invoked to
//!       receive the result, this command will return `EBUSY`.
//!
//!       When `SUCCESS` is returned, this means the request has been
//!       queued and the callback will be invoked when the CRC
//!       computation is complete.
//!
//! The CRC algorithms supported by this driver are listed below.  In
//! the values used to identify polynomials, more-significant bits
//! correspond to higher-order terms, and the most significant bit is
//! omitted because it always equals one.  All algorithms listed here
//! consume each input byte from most-significant bit to
//! least-significant.
//!
//!   * `0: CRC-32`  This algorithm is used in Ethernet and many other
//!   applications.  It uses polynomial 0x04C11DB7 and it bit-reverses
//!   and then bit-inverts the output.
//!
//!   * `1: CRC-32C`  This algorithm uses polynomial 0x1EDC6F41 (due
//!   to Castagnoli) and it bit-reverses and then bit-inverts the
//!   output.  It *may* be equivalent to various CRC functions using
//!   the same name.
//!
//!   * `2: SAM4L-16`  This algorithm uses polynomial 0x1021 and does
//!   no post-processing on the output value. The sixteen-bit CRC
//!   result is placed in the low-order bits of the returned result
//!   value, and the high-order bits will all be set.  That is, result
//!   values will always be of the form `0xFFFFxxxx` for this
//!   algorithm.  It can be performed purely in hardware on the SAM4L.
//!
//!   * `3: SAM4L-32`  This algorithm uses the same polynomial as
//!   `CRC-32`, but does no post-processing on the output value.  It
//!   can be perfomed purely in hardware on the SAM4L.
//!
//!   * `4: SAM4L-32C`  This algorithm uses the same polynomial as
//!   `CRC-32C`, but does no post-processing on the output value.  It
//!   can be performed purely in hardware on the SAM4L.

use core::cell::Cell;
use kernel::{AppId, AppSlice, Container, Callback, Driver, ReturnCode, Shared};
use kernel::hil;
use kernel::hil::crc::CrcAlg;
use kernel::process::Error;

/// An opaque value maintaining state for one application's request
#[derive(Default)]
pub struct App {
    callback: Option<Callback>,
    buffer: Option<AppSlice<Shared, u8>>,

    // if Some, the application is awaiting the result of a CRC
    //   using the given algorithm
    waiting: Option<hil::crc::CrcAlg>,
}

/// The state of the CRC driver
pub struct Crc<'a, C: hil::crc::CRC + 'a> {
    crc_unit: &'a C,
    apps: Container<App>,
    serving_app: Cell<Option<AppId>>,
}

impl<'a, C: hil::crc::CRC> Crc<'a, C> {
    /// Create a `Crc` driver
    ///
    /// The argument `crc_unit` must implement the abstract `CRC`
    /// hardware interface.  The argument `apps` should be an empty
    /// kernel `Container`, and will be used to track application
    /// requests.
    ///
    pub fn new(crc_unit: &'a C, apps: Container<App>) -> Crc<'a, C> {
        Crc {
            crc_unit: crc_unit,
            apps: apps,
            serving_app: Cell::new(None),
        }
    }

    fn serve_waiting_apps(&self) {
        if self.serving_app.get().is_some() {
            // A computation is in progress
            return;
        }

        // Find a waiting app and start its requested computation
        let mut found = false;
        for app in self.apps.iter() {
            app.enter(|app, _| {
                if let Some(alg) = app.waiting {
                    if let Some(buffer) = app.buffer.take() {
                        let r = self.crc_unit.compute(buffer.as_ref(), alg);
                        if r == ReturnCode::SUCCESS {
                            // The unit is now computing a CRC for this app
                            self.serving_app.set(Some(app.appid()));
                            found = true;
                        } else {
                            // The app's request failed
                            if let Some(mut callback) = app.callback {
                                callback.schedule(From::from(r), 0, 0);
                            }
                            app.waiting = None;
                        }

                        // Put back taken buffer
                        app.buffer = Some(buffer);
                    }
                }
            });
            if found {
                break;
            }
        }

        if !found {
            // Power down the CRC unit until next needed
            self.crc_unit.disable();
        }
    }
}

impl<'a, C: hil::crc::CRC> Driver for Crc<'a, C> {
    fn allow(&self, appid: AppId, allow_num: usize, slice: AppSlice<Shared, u8>) -> ReturnCode {
        match allow_num {
            // Provide user buffer to compute CRC over
            0 => {
                self.apps
                    .enter(appid, |app, _| {
                        app.buffer = Some(slice);
                        ReturnCode::SUCCESS
                    })
                    .unwrap_or_else(|err| match err {
                        Error::OutOfMemory => ReturnCode::ENOMEM,
                        Error::AddressOutOfBounds => ReturnCode::EINVAL,
                        Error::NoSuchApp => ReturnCode::EINVAL,
                    })
            }
            _ => ReturnCode::ENOSUPPORT,
        }
    }

    fn subscribe(&self, subscribe_num: usize, callback: Callback) -> ReturnCode {
        match subscribe_num {
            // Set callback for CRC result
            0 => {
                self.apps
                    .enter(callback.app_id(), |app, _| {
                        app.callback = Some(callback);
                        ReturnCode::SUCCESS
                    })
                    .unwrap_or_else(|err| match err {
                        Error::OutOfMemory => ReturnCode::ENOMEM,
                        Error::AddressOutOfBounds => ReturnCode::EINVAL,
                        Error::NoSuchApp => ReturnCode::EINVAL,
                    })
            }
            _ => ReturnCode::ENOSUPPORT,
        }
    }

    fn command(&self, command_num: usize, data: usize, appid: AppId) -> ReturnCode {
        match command_num {
            // This driver is present
            0 => ReturnCode::SUCCESS,

            // Get version of CRC unit
            1 => ReturnCode::SuccessWithValue { value: self.crc_unit.get_version() as usize },

            // Request a CRC computation
            2 => {
                let result = if let Some(alg) = alg_from_user_int(data) {
                    self.apps
                        .enter(appid, |app, _| {
                            if app.waiting.is_some() {
                                // Each app may make only one request at a time
                                ReturnCode::EBUSY
                            } else {
                                if app.callback.is_some() && app.buffer.is_some() {
                                    app.waiting = Some(alg);
                                    ReturnCode::SUCCESS
                                } else {
                                    ReturnCode::EINVAL
                                }
                            }
                        })
                        .unwrap_or_else(|err| match err {
                            Error::OutOfMemory => ReturnCode::ENOMEM,
                            Error::AddressOutOfBounds => ReturnCode::EINVAL,
                            Error::NoSuchApp => ReturnCode::EINVAL,
                        })
                } else {
                    ReturnCode::EINVAL
                };

                if result == ReturnCode::SUCCESS {
                    self.serve_waiting_apps();
                }
                result
            }

            _ => ReturnCode::ENOSUPPORT,
        }
    }
}

impl<'a, C: hil::crc::CRC> hil::crc::Client for Crc<'a, C> {
    fn receive_result(&self, result: u32) {
        if let Some(appid) = self.serving_app.get() {
            self.apps
                .enter(appid, |app, _| {
                    if let Some(mut callback) = app.callback {
                        callback.schedule(From::from(ReturnCode::SUCCESS), result as usize, 0);
                    }
                    app.waiting = None;
                })
                .unwrap_or_else(|err| match err {
                    Error::OutOfMemory => {}
                    Error::AddressOutOfBounds => {}
                    Error::NoSuchApp => {}
                });

            self.serving_app.set(None);
            self.serve_waiting_apps();
        } else {
            // Ignore orphaned computation
        }
    }
}

fn alg_from_user_int(i: usize) -> Option<hil::crc::CrcAlg> {
    match i {
        0 => Some(CrcAlg::Crc32),
        1 => Some(CrcAlg::Crc32C),
        2 => Some(CrcAlg::Sam4L16),
        3 => Some(CrcAlg::Sam4L32),
        4 => Some(CrcAlg::Sam4L32C),
        _ => None,
    }
}