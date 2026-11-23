// SPDX-License-Identifier: MPL-2.0
//! Platforms without a routing-table reader: pass the gateway explicitly.

use std::net::Ipv4Addr;

use crate::port_map::PortMapError;

/// Not available here.
pub(in crate::port_map) fn default_gateway_v4() -> Result<Ipv4Addr, PortMapError> {
    Err(PortMapError::Unsupported(
        "gateway discovery has no routing-table reader on this platform",
    ))
}
