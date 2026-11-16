// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod data;

// Declared first so its macros are visible in the other modules
#[macro_use]
pub mod logger;

// Implements TryFrom for each way of naming a path, for a file type that has a load(&Path)
macro_rules! load_from_paths {
    ($file:ty) => {
        impl TryFrom<&std::path::Path> for $file {
            type Error = std::io::Error;

            fn try_from(path: &std::path::Path) -> std::io::Result<Self> {
                <$file>::load(path)
            }
        }

        impl TryFrom<&std::path::PathBuf> for $file {
            type Error = std::io::Error;

            fn try_from(path: &std::path::PathBuf) -> std::io::Result<Self> {
                <$file>::load(path)
            }
        }

        impl TryFrom<std::path::PathBuf> for $file {
            type Error = std::io::Error;

            fn try_from(path: std::path::PathBuf) -> std::io::Result<Self> {
                <$file>::load(&path)
            }
        }

        impl TryFrom<&str> for $file {
            type Error = std::io::Error;

            fn try_from(path: &str) -> std::io::Result<Self> {
                <$file>::load(std::path::Path::new(path))
            }
        }
    };
}

#[cfg(feature = "sqlite")]
pub mod database;
pub mod http_request;
pub mod http_response;
pub mod image;
pub mod ip_address;
pub mod server;
pub mod tcp_listener;
pub mod text;
