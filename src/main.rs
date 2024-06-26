// SPDX-FileCopyrightText: 2023 Huang-Huang Bao
// SPDX-License-Identifier: GPL-2.0-or-later
mod config;
mod instance;
mod route;
mod skel;
mod utils;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use futures_util::StreamExt;
use ipnet::Ipv4Net;
#[cfg(feature = "ipv6")]
use ipnet::Ipv6Net;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, span, warn};

use config::{Config, ConfigNetIf, IpProtocol, NetIfId, ProtoRange};
use instance::Instance;
use route::{HairpinRouting, IfAddresses, MonitorEvent, RouteHelper};

const HELP: &str = "\
einat - An eBPF-based Endpoint-Independent NAT

USAGE:
  einat [OPTIONS]

OPTIONS:
  -h, --help                   Print this message
  -c, --config <file>          Path to configuration file
  -i, --ifname <name>          External network interface name, e.g. eth0
      --ifindex <index>        External network interface index number, e.g. 2
      --nat44                  Enable NAT44/NAPT44 for specified network interface
      --nat66                  Enable NAT66/NAPT66 for specified network interface
      --ports <range> ...      External TCP/UDP port ranges, defaults to 20000-29999
      --hairpin-if <name> ...  Hairpin internal network interface names, e.g. lo, lan0
      --bpf-log <level>        BPF tracing log level, 0 to 5, defaults to 0, disabled
";

#[derive(Default)]
struct Args {
    config_file: Option<PathBuf>,
    if_index: Option<u32>,
    if_name: Option<String>,
    nat44: bool,
    nat66: bool,
    ports: Vec<ProtoRange>,
    hairpin_if_names: Vec<String>,
    log_level: Option<u8>,
}

fn parse_env_args() -> Result<Args> {
    use lexopt::prelude::*;
    let mut args = Args::default();
    let mut parser = lexopt::Parser::from_env();
    while let Some(opt) = parser.next()? {
        match opt {
            Short('h') | Long("help") => {
                print!("{}", HELP);
                std::process::exit(0);
            }
            Short('c') | Long("config") => {
                args.config_file = Some(parser.value()?.parse()?);
            }
            Short('i') | Long("ifname") => {
                args.if_name = Some(parser.value()?.parse()?);
            }
            Long("ifindex") => {
                args.if_index = Some(parser.value()?.parse()?);
            }
            Long("nat44") => {
                args.nat44 = true;
            }
            Long("nat66") => {
                args.nat66 = true;
            }
            Long("ports") => {
                let ports: Result<Vec<_>, _> = parser.values()?.map(|s| s.parse()).collect();
                args.ports = ports?;
            }
            Long("hairpin-if") => {
                let names: Result<Vec<_>, _> = parser.values()?.map(|s| s.parse()).collect();
                args.hairpin_if_names = names?;
            }
            Long("bpf-log") => {
                args.log_level = Some(parser.value()?.parse()?);
            }
            _ => return Err(opt.unexpected().into()),
        }
    }

    Ok(args)
}

struct IfContext {
    config_idx: usize,
    if_index: u32,
    inst: Instance,
    addresses: IfAddresses,
    rt_helper: RouteHelper,
    v4_hairpin_routing: Option<HairpinRouting<Ipv4Net>>,
    #[cfg(feature = "ipv6")]
    v6_hairpin_routing: Option<HairpinRouting<Ipv6Net>>,
}

impl IfContext {
    async fn detach(&mut self) -> Result<()> {
        let mut results: Vec<Result<()>> = Vec::new();
        results.push(self.inst.detach());

        if let Some(mut hairpin_routing) = self.v4_hairpin_routing.take() {
            results.push(hairpin_routing.deconfigure().await);
        }

        #[cfg(feature = "ipv6")]
        if let Some(mut hairpin_routing) = self.v6_hairpin_routing.take() {
            results.push(hairpin_routing.deconfigure().await);
        }

        for res in results {
            res?;
        }
        Ok(())
    }
}

async fn daemon(config: &Config, contexts: &mut HashMap<u32, IfContext>) -> Result<JoinHandle<()>> {
    let (monitor_task, rt_helper, events) = route::spawn_monitor()?;

    // TODO: implement network interface(link) monitoring to attach/detach interface automatically

    let mut inst_configs = HashMap::with_capacity(config.interfaces.len());

    for (config_idx, if_config) in config.interfaces.iter().enumerate() {
        let if_index = if_config.interface.resolve_index()?;
        let link_info = rt_helper.query_link_info(if_index).await?;

        let addresses = rt_helper.query_all_addresses(if_index).await?;
        let inst_config = instance::InstanceConfig::try_from(
            if_index,
            link_info.encap(),
            if_config,
            &config.defaults,
            &addresses,
        )?;
        inst_configs.insert(if_index, (config_idx, inst_config, addresses));
    }

    let need_monitor = inst_configs
        .values()
        .any(|(_, inst_config, _)| !inst_config.is_static());

    let tasks: Vec<_> = inst_configs
        .into_iter()
        .map(|(if_index, (config_idx, inst_config, addresses))| {
            let rt_helper = rt_helper.clone();
            tokio::task::spawn_blocking(move || -> Result<_> {
                let inst = inst_config.load()?;
                Ok(IfContext {
                    config_idx,
                    if_index,
                    inst,
                    addresses,
                    rt_helper,
                    v4_hairpin_routing: Default::default(),
                    #[cfg(feature = "ipv6")]
                    v6_hairpin_routing: Default::default(),
                })
            })
        })
        .collect();

    for task in tasks {
        let ctx = task.await??;
        contexts.insert(ctx.if_index, ctx);
    }

    for ctx in contexts.values_mut() {
        ctx.inst.attach()?;

        let hairpin_config = &config.interfaces[ctx.config_idx].ipv4_hairpin_route;
        let internal_if_names = hairpin_config.internal_if_names.clone();
        let enable = hairpin_config.enable == Some(true)
            || hairpin_config.enable != Some(false) && !internal_if_names.is_empty();
        if enable {
            let ip_rule_pref = hairpin_config
                .ip_rule_pref
                .unwrap_or(config.defaults.ipv4_hairpin_rule_pref);
            let local_ip_rule_pref = config.defaults.ipv4_local_rule_pref;
            if ip_rule_pref >= local_ip_rule_pref {
                return Err(anyhow::anyhow!(
                    "Hairpin IPv4 route rule priority {} is not less than local IP rule priority {}",
                    ip_rule_pref,
                    local_ip_rule_pref,
                ));
            }

            let table_id = hairpin_config
                .table_id
                .unwrap_or(config.defaults.ipv4_hairpin_table_id)
                .get();
            let mut hairpin_routing =
                HairpinRouting::new(rt_helper.clone(), ctx.if_index, table_id);

            let res = hairpin_routing
                .configure(
                    ip_rule_pref,
                    local_ip_rule_pref,
                    internal_if_names,
                    hairpin_config.ip_protocols.clone(),
                    ctx.inst.v4_hairpin_dests(),
                )
                .await;
            match res {
                Ok(()) => ctx.v4_hairpin_routing = Some(hairpin_routing),
                Err(e) => warn!("failed to configure IPv4 hairpin routing: {}", e),
            }
        }

        #[cfg(feature = "ipv6")]
        {
            let hairpin_config = &config.interfaces[ctx.config_idx].ipv6_hairpin_route;
            let internal_if_names = hairpin_config.internal_if_names.clone();
            let enable = hairpin_config.enable == Some(true)
                || hairpin_config.enable != Some(false) && !internal_if_names.is_empty();
            if enable {
                let ip_rule_pref = hairpin_config
                    .ip_rule_pref
                    .unwrap_or(config.defaults.ipv6_hairpin_rule_pref);
                let local_ip_rule_pref = config.defaults.ipv6_local_rule_pref;
                if ip_rule_pref >= local_ip_rule_pref {
                    return Err(anyhow::anyhow!(
                        "Hairpin IPv6 route rule priority {} is not less than local IP rule priority {}",
                        ip_rule_pref,
                        local_ip_rule_pref,
                    ));
                }

                let table_id = hairpin_config
                    .table_id
                    .unwrap_or(config.defaults.ipv6_hairpin_table_id)
                    .get();
                let mut hairpin_routing =
                    HairpinRouting::new(rt_helper.clone(), ctx.if_index, table_id);
                let res = hairpin_routing
                    .configure(
                        ip_rule_pref,
                        local_ip_rule_pref,
                        internal_if_names,
                        hairpin_config.ip_protocols.clone(),
                        ctx.inst.v6_hairpin_dests(),
                    )
                    .await;
                match res {
                    Ok(()) => ctx.v6_hairpin_routing = Some(hairpin_routing),
                    Err(e) => warn!("failed to configure IPv6 hairpin routing: {}", e),
                }
            }
        }
    }

    drop(rt_helper);

    let monitor = async {
        if !need_monitor {
            std::future::pending::<()>().await;
            return Ok(());
        }

        futures_util::pin_mut!(events);
        while let Some(event) = events.next().await {
            let MonitorEvent::ChangeAddress { if_index } = event;

            if let Some(ctx) = contexts.get_mut(&if_index) {
                let new_addresses = ctx.rt_helper.query_all_addresses(if_index).await?;
                if new_addresses.ipv4 != ctx.addresses.ipv4 {
                    debug!(
                        "IPv4 addresses {:?} -> {:?}",
                        ctx.addresses.ipv4, new_addresses.ipv4
                    );
                    ctx.inst.reconfigure_v4_addresses(&new_addresses.ipv4)?;
                    ctx.addresses.ipv4 = new_addresses.ipv4;
                }
                #[cfg(feature = "ipv6")]
                if new_addresses.ipv6 != ctx.addresses.ipv6 {
                    debug!(
                        "IPv6 addresses {:?} -> {:?}",
                        ctx.addresses.ipv6, new_addresses.ipv6
                    );
                    ctx.inst.reconfigure_v6_addresses(&new_addresses.ipv6)?;
                    ctx.addresses.ipv6 = new_addresses.ipv6;
                }

                if let Some(hairpin_routing) = &mut ctx.v4_hairpin_routing {
                    if let Err(e) = hairpin_routing
                        .reconfigure_dests(ctx.inst.v4_hairpin_dests())
                        .await
                    {
                        error!("failed to reconfigure IPv4 hairpin routing: {}", e);
                    }
                }

                #[cfg(feature = "ipv6")]
                if let Some(hairpin_routing) = &mut ctx.v6_hairpin_routing {
                    if let Err(e) = hairpin_routing
                        .reconfigure_dests(ctx.inst.v6_hairpin_dests())
                        .await
                    {
                        error!("failed to reconfigure IPv6 hairpin routing: {}", e);
                    }
                }
            }
        }

        Result::<()>::Ok(())
    };

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = sigint.recv() => {
            Result::<()>::Ok(())
        }
        _ = sigterm.recv() => {
            Result::<()>::Ok(())
        }
        res = monitor => {
            res
        }
    }?;

    Ok(monitor_task)
}

async fn daemon_guard(config: &Config) -> Result<()> {
    let mut contexts: HashMap<u32, IfContext> = HashMap::with_capacity(config.interfaces.len());

    let res = daemon(config, &mut contexts).await;

    for ctx in contexts.values_mut() {
        if let Err(e) = ctx.detach().await {
            error!("failed to cleanup context: {}", e);
        };
    }

    res?.abort();
    Ok(())
}

fn tracing_init() -> Result<()> {
    use libbpf_rs::PrintLevel;

    tracing_subscriber::fmt::init();

    libbpf_rs::set_print(Some((PrintLevel::Debug, |level, msg| {
        let span = span!(tracing::Level::DEBUG, "libbpf");
        let _enter = span.enter();

        let msg = msg.trim_end_matches('\n');
        match level {
            PrintLevel::Info => info!("{}", msg),
            PrintLevel::Warn => warn!("{}", msg),
            PrintLevel::Debug => debug!("{}", msg),
        }
    })));

    Ok(())
}

fn main() -> Result<()> {
    tracing_init()?;

    let args = parse_env_args()?;

    let mut config: Config = if let Some(config_path) = &args.config_file {
        let text = std::fs::read_to_string(config_path)?;
        toml::from_str(&text)?
    } else {
        Config::default()
    };

    if args.if_index.is_some() || args.if_name.is_some() {
        if args.config_file.is_some() {
            return Err(anyhow::anyhow!(
                "Combining interface configuration from CLI options with configuration file is not allowed"
            ));
        }

        let interface = if let Some(if_index) = args.if_index {
            NetIfId::Index { if_index }
        } else if let Some(if_name) = args.if_name {
            NetIfId::Name { if_name }
        } else {
            unreachable!()
        };

        let nat44 = args.nat44 || !args.nat66;
        let nat66 = args.nat66;

        #[cfg(not(feature = "ipv6"))]
        if nat66 {
            warn!("NAT66 feature not enabled for this build, ignoring");
        }

        if !args.ports.is_empty() {
            config.defaults.tcp_ranges = args.ports.clone();
            config.defaults.udp_ranges = args.ports;
        }

        let hairpin_route = config::ConfigHairpinRoute {
            enable: None,
            internal_if_names: args.hairpin_if_names,
            ip_rule_pref: None,
            table_id: None,
            ip_protocols: vec![IpProtocol::Tcp, IpProtocol::Udp],
        };

        let if_config = ConfigNetIf {
            interface,
            bpf_log_level: args.log_level,
            nat44,
            nat66,
            default_externals: true,
            ipv4_hairpin_route: hairpin_route.clone(),
            ipv6_hairpin_route: hairpin_route,
            ..Default::default()
        };

        config.interfaces = vec![if_config];
    }

    if config.interfaces.is_empty() {
        return Err(anyhow::anyhow!("No network interface specified"));
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(daemon_guard(&config))
}
