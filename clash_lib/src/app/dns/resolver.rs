use async_trait::async_trait;
use futures::{FutureExt, TryFutureExt};
use rand::prelude::SliceRandom;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;
use std::{net, sync::Arc};
use tokio::sync::RwLock;
use tracing::{debug, instrument, warn};

use hickory_proto::{op, rr};

use crate::app::profile::ThreadSafeCacheFile;
use crate::common::mmdb::MMDB;
use crate::config::def::DNSMode;
use crate::dns::helper::make_clients;
use crate::dns::ThreadSafeDNSClient;
use crate::dns_debug;
use crate::{common::trie, Error};

use super::fakeip::{self, FileStore, InMemStore, ThreadSafeFakeDns};
use super::system::SystemResolver;
use super::{
    filters::{DomainFilter, FallbackDomainFilter, FallbackIPFilter, GeoIPFilter, IPNetFilter},
    Config,
};
use super::{ClashResolver, ResolverKind, ThreadSafeDNSResolver};

static TTL: Duration = Duration::from_secs(60);

pub struct Resolver {
    ipv6: AtomicBool,
    hosts: Option<trie::StringTrie<net::IpAddr>>,
    main: Vec<ThreadSafeDNSClient>,

    fallback: Option<Vec<ThreadSafeDNSClient>>,
    fallback_domain_filters: Option<Vec<Box<dyn FallbackDomainFilter>>>,
    fallback_ip_filters: Option<Vec<Box<dyn FallbackIPFilter>>>,

    lru_cache: Option<Arc<RwLock<lru_time_cache::LruCache<String, op::Message>>>>,
    policy: Option<trie::StringTrie<Vec<ThreadSafeDNSClient>>>,

    fake_dns: Option<ThreadSafeFakeDns>,
}

impl Resolver {
    /// For testing purpose
    #[cfg(test)]
    pub async fn new_default() -> Self {
        use crate::app::dns::dns_client::DNSNetMode;

        use super::config::NameServer;

        Resolver {
            ipv6: AtomicBool::new(false),
            hosts: None,
            main: make_clients(
                vec![NameServer {
                    net: DNSNetMode::UDP,
                    address: "8.8.8.8:53".to_string(),
                    interface: None,
                }],
                None,
            )
            .await,
            fallback: None,
            fallback_domain_filters: None,
            fallback_ip_filters: None,
            lru_cache: None,
            policy: None,

            fake_dns: None,
        }
    }

    pub async fn new(
        cfg: &Config,
        store: ThreadSafeCacheFile,
        mmdb: Arc<MMDB>,
    ) -> ThreadSafeDNSResolver {
        if !cfg.enable {
            return Arc::new(SystemResolver::new().expect("failed to create system resolver"));
        }

        let default_resolver = Arc::new(Resolver {
            ipv6: AtomicBool::new(false),
            hosts: None,
            main: make_clients(cfg.default_nameserver.clone(), None).await,
            fallback: None,
            fallback_domain_filters: None,
            fallback_ip_filters: None,
            lru_cache: None,
            policy: None,

            fake_dns: None,
        });

        let r = Resolver {
            ipv6: AtomicBool::new(cfg.ipv6),
            main: make_clients(cfg.nameserver.clone(), Some(default_resolver.clone())).await,
            hosts: cfg.hosts.clone(),
            fallback: if cfg.fallback.len() > 0 {
                Some(make_clients(cfg.fallback.clone(), Some(default_resolver.clone())).await)
            } else {
                None
            },
            fallback_domain_filters: if cfg.fallback_filter.domain.len() > 0 {
                Some(vec![Box::new(DomainFilter::new(
                    cfg.fallback_filter
                        .domain
                        .iter()
                        .map(|x| x.as_str())
                        .collect(),
                )) as Box<dyn FallbackDomainFilter>])
            } else {
                None
            },
            fallback_ip_filters: if cfg.fallback_filter.ip_cidr.is_some()
                || cfg.fallback_filter.geo_ip
            {
                let mut filters = vec![];

                filters.push(
                    Box::new(GeoIPFilter::new(&cfg.fallback_filter.geo_ip_code, mmdb))
                        as Box<dyn FallbackIPFilter>,
                );

                if let Some(ipcidr) = &cfg.fallback_filter.ip_cidr {
                    for subnet in ipcidr {
                        filters
                            .push(Box::new(IPNetFilter::new(*subnet)) as Box<dyn FallbackIPFilter>)
                    }
                }

                Some(filters)
            } else {
                None
            },
            lru_cache: Some(Arc::new(RwLock::new(
                lru_time_cache::LruCache::with_expiry_duration_and_capacity(TTL, 4096),
            ))),
            policy: if cfg.nameserver_policy.len() > 0 {
                let mut p = trie::StringTrie::new();
                for (domain, ns) in &cfg.nameserver_policy {
                    p.insert(
                        domain.as_str(),
                        Arc::new(
                            make_clients(vec![ns.to_owned()], Some(default_resolver.clone())).await,
                        ),
                    );
                }
                Some(p)
            } else {
                None
            },
            fake_dns: match cfg.enhance_mode {
                DNSMode::FakeIp => Some(Arc::new(RwLock::new(
                    fakeip::FakeDns::new(fakeip::Opts {
                        ipnet: cfg.fake_ip_range,
                        skipped_hostnames: if cfg.fake_ip_filter.len() != 0 {
                            let mut host = trie::StringTrie::new();
                            for domain in cfg.fake_ip_filter.iter() {
                                host.insert(domain.as_str(), Arc::new(true));
                            }
                            Some(host)
                        } else {
                            None
                        },
                        store: if cfg.store_fake_ip {
                            Box::new(FileStore::new(store))
                        } else {
                            Box::new(InMemStore::new(1000))
                        },
                    })
                    .unwrap(),
                ))),
                DNSMode::RedirHost => {
                    warn!("dns redir-host is not supported and will not do anything");
                    None
                }
                _ => None,
            },
        };

        Arc::new(r)
    }

    pub async fn batch_exchange(
        clients: &Vec<ThreadSafeDNSClient>,
        message: &op::Message,
    ) -> anyhow::Result<op::Message> {
        let mut queries = Vec::new();
        for c in clients {
            queries.push(
                async move {
                    c.exchange(message)
                        .inspect_err(|x| {
                            debug!("DNS client {} resolve error: {}", c.id(), x.to_string())
                        })
                        .await
                }
                .boxed(),
            )
        }

        let timeout = tokio::time::sleep(Duration::from_secs(10));

        tokio::select! {
            result = futures::future::select_ok(queries) => match result {
                Ok(r) => Ok(r.0),
                Err(e) => Err(e.into()),
            },
            _ = timeout => Err(Error::DNSError("DNS query timeout".into()).into())
        }
    }

    /// guaranteed to return at least 1 IP address when Ok
    async fn lookup_ip(
        &self,
        host: &str,
        record_type: rr::record_type::RecordType,
    ) -> anyhow::Result<Vec<net::IpAddr>> {
        let mut m = op::Message::new();
        let mut q = op::Query::new();
        let name = rr::Name::from_str_relaxed(host)
            .map_err(|_x| anyhow!("invalid domain: {}", host))?
            .append_domain(&rr::Name::root())?; // makes it FQDN
        q.set_name(name);
        q.set_query_type(record_type);
        m.add_query(q);
        m.set_recursion_desired(true);

        match self.exchange(m).await {
            Ok(result) => {
                let ip_list = Resolver::ip_list_of_message(&result);
                if !ip_list.is_empty() {
                    Ok(ip_list)
                } else {
                    Err(anyhow!("no record for hostname: {}", host))
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn exchange(&self, message: op::Message) -> anyhow::Result<op::Message> {
        if let Some(q) = message.query() {
            if let Some(lru) = &self.lru_cache {
                if let Some(cached) = lru.read().await.peek(q.to_string().as_str()) {
                    return Ok(cached.clone());
                }
            }
            self.exchange_no_cache(&message).await
        } else {
            Err(anyhow!("invalid query"))
        }
    }

    async fn exchange_no_cache(&self, message: &op::Message) -> anyhow::Result<op::Message> {
        let q = message.query().unwrap();

        let query = async move {
            if Resolver::is_ip_request(q) {
                return self.ip_exchange(message).await;
            }

            if let Some(matched) = self.match_policy(&message) {
                return Resolver::batch_exchange(&matched, message).await;
            }

            return Resolver::batch_exchange(&self.main, message).await;
        };

        let rv = query.await;

        if let Ok(msg) = &rv {
            if let Some(lru) = &self.lru_cache {
                if !(q.query_type() == rr::RecordType::TXT
                    && q.name().to_ascii().starts_with("_acme-challenge."))
                {
                    // TODO: make this TTL wired to LRU cache
                    #[allow(unused_variables)]
                    let ttl = if msg.answer_count() != 0 {
                        msg.answers()
                            .iter()
                            .map(|x| x.ttl())
                            .min()
                            .unwrap_or_default()
                    } else if msg.name_server_count() != 0 {
                        msg.name_servers()
                            .iter()
                            .map(|x| x.ttl())
                            .min()
                            .unwrap_or_default()
                    } else {
                        msg.additionals()
                            .iter()
                            .map(|x| x.ttl())
                            .min()
                            .unwrap_or_default()
                    };

                    lru.write().await.insert(q.to_string(), msg.clone());
                }
            }
        }

        return rv;
    }

    fn match_policy(&self, m: &op::Message) -> Option<&Vec<ThreadSafeDNSClient>> {
        if let (Some(_fallback), Some(_fallback_domain_filters), Some(policy)) =
            (&self.fallback, &self.fallback_domain_filters, &self.policy)
        {
            if let Some(domain) = Resolver::domain_name_of_message(m) {
                return policy.search(&domain).map(|n| n.get_data().unwrap());
            }
        }
        None
    }

    async fn ip_exchange(&self, message: &op::Message) -> anyhow::Result<op::Message> {
        if let Some(mut matched) = self.match_policy(message) {
            return Resolver::batch_exchange(&mut matched, message).await;
        }

        if self.should_only_query_fallback(message) {
            // self.fallback guaranteed in the above check
            return Resolver::batch_exchange(&self.fallback.as_ref().unwrap(), message).await;
        }

        let main_query = Resolver::batch_exchange(&self.main, message);

        if self.fallback.is_none() {
            return main_query.await;
        }

        let fallback_query = Resolver::batch_exchange(&self.fallback.as_ref().unwrap(), message);

        if let Ok(main_result) = main_query.await {
            let ip_list = Resolver::ip_list_of_message(&main_result);
            if !ip_list.is_empty() {
                // TODO: only check 1st?
                if !self.should_ip_fallback(&ip_list[0]) {
                    return Ok(main_result);
                }
            }
        }

        fallback_query.await
    }

    fn should_only_query_fallback(&self, message: &op::Message) -> bool {
        if let (Some(_), Some(fallback_domain_filters)) =
            (&self.fallback, &self.fallback_domain_filters)
        {
            if let Some(domain) = Resolver::domain_name_of_message(message) {
                for f in fallback_domain_filters.into_iter() {
                    if f.apply(domain.as_str()) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn should_ip_fallback(&self, ip: &net::IpAddr) -> bool {
        if let Some(filers) = &self.fallback_ip_filters {
            for f in filers.iter() {
                if f.apply(ip) {
                    return true;
                }
            }
        }
        false
    }

    // helpers
    fn is_ip_request(q: &op::Query) -> bool {
        q.query_class() == rr::DNSClass::IN
            && (q.query_type() == rr::RecordType::A || q.query_type() == rr::RecordType::AAAA)
    }

    fn domain_name_of_message(m: &op::Message) -> Option<String> {
        m.query()
            .map(|x| x.name().to_ascii().trim_matches('.').to_owned())
    }

    pub(crate) fn ip_list_of_message(m: &op::Message) -> Vec<net::IpAddr> {
        m.answers()
            .into_iter()
            .filter(|r| {
                r.record_type() == rr::RecordType::A || r.record_type() == rr::RecordType::AAAA
            })
            .map(|r| match r.data() {
                Some(data) => match data {
                    rr::RData::A(v4) => net::IpAddr::V4(**v4),
                    rr::RData::AAAA(v6) => net::IpAddr::V6(**v6),
                    _ => unreachable!("should be only A/AAAA"),
                },
                None => unreachable!("should only be A/AAAA"),
            })
            .collect()
    }
}

#[async_trait]
impl ClashResolver for Resolver {
    #[instrument(skip(self))]
    async fn resolve(&self, host: &str, enhanced: bool) -> anyhow::Result<Option<net::IpAddr>> {
        match self.ipv6.load(Relaxed) {
            true => {
                let fut1 = self
                    .resolve_v6(host, enhanced)
                    .map(|x| x.map(|v6| v6.map(|v6| net::IpAddr::from(v6))));
                let fut2 = self
                    .resolve_v4(host, enhanced)
                    .map(|x| x.map(|v4| v4.map(|v4| net::IpAddr::from(v4))));

                let futs = vec![fut1.boxed(), fut2.boxed()];
                let r = futures::future::select_ok(futs).await?;
                if r.0.is_some() {
                    return Ok(r.0);
                }
                let r = futures::future::select_all(r.1).await;
                return r.0;
            }
            false => self
                .resolve_v4(host, enhanced)
                .await
                .map(|ip| ip.map(|v4| net::IpAddr::from(v4))),
        }
    }
    async fn resolve_v4(
        &self,
        host: &str,
        enhanced: bool,
    ) -> anyhow::Result<Option<net::Ipv4Addr>> {
        if enhanced {
            if let Some(hosts) = &self.hosts {
                if let Some(v) = hosts.search(host) {
                    return Ok(v.get_data().map(|v| match v {
                        net::IpAddr::V4(v4) => *v4,
                        _ => unreachable!("invalid IP family"),
                    }));
                }
            }
        }

        if let Ok(ip) = host.parse::<net::Ipv4Addr>() {
            return Ok(Some(ip));
        }

        if enhanced && self.fake_ip_enabled() {
            let mut fake_dns = self.fake_dns.as_ref().unwrap().write().await;
            if !fake_dns.should_skip(host) {
                let ip = fake_dns.lookup(host).await;
                dns_debug!("fake dns lookup: {} -> {:?}", host, ip);
                match ip {
                    net::IpAddr::V4(v4) => return Ok(Some(v4)),
                    _ => unreachable!("invalid IP family"),
                }
            }
        }

        match self.lookup_ip(host, rr::RecordType::A).await {
            Ok(result) => match result.choose(&mut rand::thread_rng()).unwrap() {
                net::IpAddr::V4(v4) => Ok(Some(*v4)),
                _ => unreachable!("invalid IP family"),
            },
            Err(e) => Err(e.into()),
        }
    }

    async fn resolve_v6(
        &self,
        host: &str,
        enhanced: bool,
    ) -> anyhow::Result<Option<net::Ipv6Addr>> {
        if !self.ipv6.load(Relaxed) {
            return Err(Error::DNSError("ipv6 disabled".into()).into());
        }

        if enhanced {
            if let Some(hosts) = &self.hosts {
                if let Some(v) = hosts.search(host) {
                    return Ok(v.get_data().map(|v| match v {
                        net::IpAddr::V6(v6) => *v6,
                        _ => unreachable!("invalid IP family"),
                    }));
                }
            }
        }

        if let Ok(ip) = host.parse::<net::Ipv6Addr>() {
            return Ok(Some(ip));
        }

        match self.lookup_ip(host, rr::RecordType::AAAA).await {
            Ok(result) => match result.choose(&mut rand::thread_rng()).unwrap() {
                net::IpAddr::V6(v6) => Ok(Some(*v6)),
                _ => unreachable!("invalid IP family"),
            },

            Err(e) => Err(e.into()),
        }
    }

    async fn exchange(&self, message: op::Message) -> anyhow::Result<op::Message> {
        self.exchange(message).await
    }

    fn ipv6(&self) -> bool {
        self.ipv6.load(Relaxed)
    }

    fn set_ipv6(&self, enable: bool) {
        self.ipv6.store(enable, Relaxed);
    }

    fn kind(&self) -> ResolverKind {
        ResolverKind::Clash
    }

    fn fake_ip_enabled(&self) -> bool {
        self.fake_dns.is_some()
    }

    async fn is_fake_ip(&self, ip: std::net::IpAddr) -> bool {
        if !self.fake_ip_enabled() {
            return false;
        }

        let mut fake_dns = self.fake_dns.as_ref().unwrap().write().await;
        fake_dns.is_fake_ip(ip).await
    }

    async fn fake_ip_exists(&self, ip: std::net::IpAddr) -> bool {
        if !self.fake_ip_enabled() {
            return false;
        }

        let mut fake_dns = self.fake_dns.as_ref().unwrap().write().await;
        fake_dns.exist(ip).await
    }

    async fn reverse_lookup(&self, ip: net::IpAddr) -> Option<String> {
        dns_debug!("reverse lookup: {}", ip);
        if !self.fake_ip_enabled() {
            return None;
        }

        let mut fake_dns = self.fake_dns.as_ref().unwrap().write().await;
        fake_dns.reverse_lookup(ip).await
    }
}

#[cfg(test)]
mod tests {
    use crate::dns::dns_client::{DNSNetMode, DnsClient, Opts};
    use crate::dns::{Resolver, ThreadSafeDNSClient};
    use hickory_client::{client, op};
    use hickory_proto::rr;
    use hickory_proto::udp::UdpClientStream;
    use hickory_proto::xfer::{DnsHandle, DnsRequest, DnsRequestOptions, FirstAnswer};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn test_bad_labels_with_custom_resolver() {
        let name = rr::Name::from_str_relaxed("some_domain.understore")
            .unwrap()
            .append_domain(&rr::Name::root())
            .unwrap();
        assert_eq!(name.to_string(), "some_domain.understore.");

        let mut m = op::Message::new();
        let mut q = op::Query::new();

        q.set_name(name);
        q.set_query_type(rr::RecordType::A);
        m.add_query(q);
        m.set_recursion_desired(true);

        let stream = UdpClientStream::<UdpSocket>::with_timeout(
            "1.1.1.1:53".parse().unwrap(),
            Duration::from_secs(5),
        );
        let (client, bg) = client::AsyncClient::connect(stream).await.unwrap();

        tokio::spawn(bg);

        let mut req = DnsRequest::new(m, DnsRequestOptions::default());
        req.set_id(rand::random::<u16>());
        let res = client.send(req).first_answer().await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_udp_resolve() {
        let c = DnsClient::new(Opts {
            r: None,
            host: "114.114.114.114".to_string(),
            port: 53,
            net: DNSNetMode::UDP,
            iface: None,
        })
        .await
        .expect("build client");

        test_client(c).await;
    }

    #[tokio::test]
    async fn test_tcp_resolve() {
        let c = DnsClient::new(Opts {
            r: None,
            host: "1.1.1.1".to_string(),
            port: 53,
            net: DNSNetMode::TCP,
            iface: None,
        })
        .await
        .expect("build client");

        test_client(c).await;
    }

    #[tokio::test]
    #[ignore = "network unstable on CI"]
    async fn test_dot_resolve() {
        let c = DnsClient::new(Opts {
            r: Some(Arc::new(Resolver::new_default().await)),
            host: "dns.google".to_string(),
            port: 853,
            net: DNSNetMode::DoT,
            iface: None,
        })
        .await
        .expect("build client");

        test_client(c).await;
    }

    #[tokio::test]
    #[ignore = "network unstable on CI"]
    async fn test_doh_resolve() {
        let default_resolver = Arc::new(Resolver::new_default().await);

        let c = DnsClient::new(Opts {
            r: Some(default_resolver.clone()),
            host: "cloudflare-dns.com".to_string(),
            port: 443,
            net: DNSNetMode::DoH,
            iface: None,
        })
        .await
        .expect("build client");

        test_client(c).await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_dhcp_client() {
        let c = DnsClient::new(Opts {
            r: None,
            host: "en0".to_string(),
            port: 0,
            net: DNSNetMode::DHCP,
            iface: None,
        })
        .await
        .expect("build client");

        test_client(c).await;
    }

    async fn test_client(c: ThreadSafeDNSClient) -> () {
        let mut m = op::Message::new();
        let mut q = op::Query::new();
        q.set_name(rr::Name::from_utf8("www.google.com").unwrap());
        q.set_query_type(rr::RecordType::A);
        m.add_query(q);

        let r = Resolver::batch_exchange(&vec![c.clone()], &m)
            .await
            .expect("should exchange");

        let ips = Resolver::ip_list_of_message(&r);

        assert!(ips.len() > 0);
        assert!(!ips[0].is_unspecified());
        assert!(ips[0].is_ipv4());

        let mut m = op::Message::new();
        let mut q = op::Query::new();
        q.set_name(rr::Name::from_utf8("www.google.com").unwrap());
        q.set_query_type(rr::RecordType::AAAA);
        m.add_query(q);

        let r = Resolver::batch_exchange(&vec![c.clone()], &m)
            .await
            .expect("should exchange");

        let ips = Resolver::ip_list_of_message(&r);

        assert!(ips.len() > 0);
        assert!(!ips[0].is_unspecified());
        assert!(ips[0].is_ipv6());
    }
}
