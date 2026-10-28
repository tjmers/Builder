pub type Ipv4 = [u8; 4];
pub type Ipv6 = [u8; 16];

#[derive(Debug, Clone, Copy)]
pub enum IpAddress {
    V4(Ipv4),
    V6(Ipv6),
}

// Copies
impl From<Ipv4> for IpAddress {
    fn from(ipv4: Ipv4) -> Self {
        IpAddress::V4(ipv4)
    }
}

impl From<Ipv6> for IpAddress {
    fn from(ipv6: Ipv6) -> Self {
        IpAddress::V6(ipv6)
    }
}
