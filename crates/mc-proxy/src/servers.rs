//! Backend server registry and hostname routing.

use std::collections::HashMap;
use std::net::SocketAddr;

use crate::config::{Config, ConfigError};
use crate::protocol::handshake::clean_address;

/// A backend server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    /// Logical name.
    pub name: String,
    /// `host:port` as configured.
    pub address: String,
}

impl Server {
    /// Resolve the address into a socket address.
    pub async fn resolve(&self) -> std::io::Result<SocketAddr> {
        let addr = tokio::net::lookup_host(self.address.as_str())
            .await?
            .next()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    format!("could not resolve backend address '{}'", self.address),
                )
            })?;
        Ok(addr)
    }
}

/// All configured servers plus routing rules.
#[derive(Debug, Clone)]
pub struct ServerRegistry {
    servers: HashMap<String, Server>,
    order: Vec<String>,
    forced_hosts: HashMap<String, String>,
    try_servers: Vec<String>,
}

impl ServerRegistry {
    /// Build the registry from a validated configuration.
    pub fn from_config(config: &Config) -> Result<Self, ConfigError> {
        let mut servers = HashMap::new();
        let mut order = Vec::new();
        for entry in &config.servers {
            order.push(entry.name.clone());
            servers.insert(
                entry.name.clone(),
                Server {
                    name: entry.name.clone(),
                    address: entry.address.clone(),
                },
            );
        }
        let forced_hosts = config
            .forced_hosts
            .iter()
            .map(|(host, server)| (clean_address(host), server.clone()))
            .collect();
        Ok(Self {
            servers,
            order,
            forced_hosts,
            try_servers: config.try_servers.servers.clone(),
        })
    }

    /// The default server (first `[[servers]]` entry).
    pub fn default_server(&self) -> &Server {
        let name = self
            .order
            .first()
            .expect("configuration validation guarantees at least one server");
        &self.servers[name]
    }

    /// Look up a server by name.
    pub fn get(&self, name: &str) -> Option<&Server> {
        self.servers.get(name)
    }

    /// Route a virtual host to a server.
    pub fn route(&self, host: &str) -> &Server {
        let host = clean_address(host);
        if let Some(name) = self.forced_hosts.get(&host) {
            if let Some(server) = self.servers.get(name) {
                return server;
            }
        }
        self.default_server()
    }

    /// Candidate servers for a new connection, in order.
    ///
    /// The routed server comes first, followed by the `try` list and finally
    /// the remaining configured servers. Duplicates are removed.
    pub fn candidates(&self, host: &str) -> Vec<&Server> {
        let mut result: Vec<&Server> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let routed = self.route(host);
        result.push(routed);
        seen.insert(routed.name.as_str());
        for name in &self.try_servers {
            if let Some(server) = self.servers.get(name) {
                if seen.insert(server.name.as_str()) {
                    result.push(server);
                }
            }
        }
        for name in &self.order {
            if let Some(server) = self.servers.get(name) {
                if seen.insert(server.name.as_str()) {
                    result.push(server);
                }
            }
        }
        result
    }

    /// All servers in configuration order.
    pub fn iter(&self) -> impl Iterator<Item = &Server> {
        self.order.iter().filter_map(|name| self.servers.get(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ServerRegistry {
        let config = Config::from_toml(
            r#"
[[servers]]
name = "lobby"
address = "127.0.0.1:25566"

[[servers]]
name = "survival"
address = "127.0.0.1:25567"

[forced_hosts]
"survival.example.com" = "survival"

[try]
servers = ["survival"]
"#,
        )
        .expect("config should parse");
        ServerRegistry::from_config(&config).unwrap()
    }

    #[test]
    fn routes_forced_hosts_and_default() {
        let registry = registry();
        assert_eq!(registry.route("survival.example.com").name, "survival");
        assert_eq!(registry.route("SURVIVAL.EXAMPLE.COM.").name, "survival");
        assert_eq!(registry.route("unknown.example.com").name, "lobby");
    }

    #[test]
    fn candidates_prefer_routed_then_try() {
        let registry = registry();
        let names: Vec<&str> = registry
            .candidates("survival.example.com")
            .iter()
            .map(|server| server.name.as_str())
            .collect();
        assert_eq!(names, vec!["survival", "lobby"]);
    }
}
