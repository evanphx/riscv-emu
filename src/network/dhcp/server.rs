//! DHCP server with static client assignments.

use super::*;
use crate::network::Mac;
use std::collections::HashMap;
use std::net::Ipv4Addr;

/// Static configuration of a DHCP client.
#[derive(Clone, Debug)]
pub struct ClientInfo {
    pub ip: Ipv4Addr,
    pub hostname: String,
}

impl ClientInfo {
    /// Returns the domain part of the client hostname.
    pub fn domain_name(&self) -> &str {
        match self.hostname.find('.') {
            Some(idx) => &self.hostname[idx + 1..],
            None => "",
        }
    }
}

/// DHCP server.
#[derive(Clone, Debug)]
pub struct Server {
    pub hostname: NullString,
    /// Subnet mask; the default mask of the gateway if unspecified.
    pub netmask: Ipv4Addr,
    /// Broadcast address; derived from the gateway if unspecified.
    pub broadcast: Ipv4Addr,
    pub gw: Ipv4Addr,
    pub domain_name: String,
    pub dns: Vec<Ipv4Addr>,
    pub clients: HashMap<Mac, ClientInfo>,
}

impl Server {
    /// Creates a new server with the hostname and gateway address.
    pub fn new(hostname: &str, gw: Ipv4Addr) -> Server {
        Server {
            hostname: NullString::new(hostname),
            netmask: Ipv4Addr::UNSPECIFIED,
            broadcast: Ipv4Addr::UNSPECIFIED,
            gw,
            domain_name: String::new(),
            dns: Vec::new(),
            clients: HashMap::new(),
        }
    }

    /// Adds a static client configuration.
    pub fn add_client(&mut self, mac: Mac, client: ClientInfo) {
        self.clients.insert(mac, client);
    }

    /// Processes the client request req and returns the server
    /// response. The response is None if the server ignored the
    /// request.
    pub fn request(&self, req: &Dhcp) -> Result<Option<Dhcp>, String> {
        if req.op != BOOTREQUEST {
            return Err(format!("invalid DHCPREQUEST op {}", req.op));
        }
        if req.chaddr.0.len() < 6 {
            return Err(format!("invalid client address {}", req.chaddr));
        }
        let client_mac = Mac::from_slice(&req.chaddr.0);
        let client = self
            .clients
            .get(&client_mac)
            .ok_or_else(|| format!("unknown client {}", client_mac))?;

        let (ciaddr, msg_type) = match req.msg_type {
            DHCPDISCOVER => (Ipv4Addr::UNSPECIFIED, DHCPOFFER),
            DHCPREQUEST => (req.ciaddr, DHCPACK),
            _ => return Ok(None),
        };
        let mut resp = Dhcp {
            op: BOOTREPLY,
            htype: req.htype,
            hlen: 6,
            xid: req.xid,
            flags: req.flags,
            ciaddr,
            yiaddr: client.ip,
            siaddr: self.gw,
            giaddr: req.giaddr,
            chaddr: req.chaddr.clone(),
            sname: self.hostname.clone(),
            cookie: OPTIONS_COOKIE,
            msg_type,
            ..Default::default()
        };

        // Add mandatory server options.
        resp.add_option(DhcpOption::new(TAG_DHCP_MSG_TYPE, &[msg_type.0]));
        // IP Address Lease Time.
        resp.add_option(DhcpOption::new(TAG_ADDRESS_TIME, &86400u32.to_be_bytes()));
        resp.add_option(DhcpOption::new(TAG_DHCP_SERVER_ID, &self.gw.octets()));

        // Process request options.
        for opt in req.options.iter().filter(|o| o.tag == TAG_PARAMETER_LIST) {
            for (pi, &p) in opt.data.iter().enumerate() {
                match OptionTag(p) {
                    TAG_SUBNET_MASK => {
                        resp.add_option(DhcpOption::new(TAG_SUBNET_MASK, &self.netmask_data()))
                    }
                    TAG_BROADCAST_ADDRESS => resp.add_option(DhcpOption::new(
                        TAG_BROADCAST_ADDRESS,
                        &self.broadcast_addr().octets(),
                    )),
                    TAG_ROUTER => resp.add_option(DhcpOption::new(TAG_ROUTER, &self.gw.octets())),
                    TAG_DOMAIN_SERVER => {
                        if !self.dns.is_empty() {
                            let data: Vec<u8> = self.dns.iter().flat_map(|d| d.octets()).collect();
                            resp.add_option(DhcpOption::new(TAG_DOMAIN_SERVER, &data));
                        }
                    }
                    TAG_HOSTNAME => {
                        if !client.hostname.is_empty() {
                            resp.add_option(DhcpOption::new(
                                TAG_HOSTNAME,
                                client.hostname.as_bytes(),
                            ));
                        }
                    }
                    TAG_DOMAIN_NAME => {
                        let mut name = client.domain_name();
                        if name.is_empty() {
                            name = self.server_domain_name();
                        }
                        if !name.is_empty() {
                            resp.add_option(DhcpOption::new(TAG_DOMAIN_NAME, name.as_bytes()));
                        }
                    }
                    tag => {
                        if option_type(tag).is_none() {
                            eprintln!("ignoring unknown parameter {}: {}", pi, p);
                        }
                    }
                }
            }
        }

        resp.add_option(DhcpOption::new(TAG_END, &[]));

        Ok(Some(resp))
    }

    /// Returns the subnet mask: the configured netmask or the default
    /// classful mask of the gateway address.
    fn netmask_data(&self) -> Vec<u8> {
        if !self.netmask.is_unspecified() {
            return self.netmask.octets().to_vec();
        }
        let a = self.gw.octets()[0];
        let mask: &[u8] = if a < 0x80 {
            &[0xff, 0, 0, 0]
        } else if a < 0xc0 {
            &[0xff, 0xff, 0, 0]
        } else if a < 0xe0 {
            &[0xff, 0xff, 0xff, 0]
        } else {
            &[]
        };
        mask.to_vec()
    }

    /// Returns the broadcast address: the configured address or the
    /// gateway address with its last byte set to 0xff.
    fn broadcast_addr(&self) -> Ipv4Addr {
        if !self.broadcast.is_unspecified() {
            return self.broadcast;
        }
        if self.gw.is_unspecified() {
            return self.gw;
        }
        let mut b = self.gw.octets();
        b[3] = 0xff;
        Ipv4Addr::from(b)
    }

    /// Returns the configured domain name or the domain part of the
    /// server hostname.
    fn server_domain_name(&self) -> &str {
        if !self.domain_name.is_empty() {
            return &self.domain_name;
        }
        let host = std::str::from_utf8(self.hostname.as_bytes()).unwrap_or("");
        match host.find('.') {
            Some(idx) => &host[idx + 1..],
            None => "",
        }
    }
}
