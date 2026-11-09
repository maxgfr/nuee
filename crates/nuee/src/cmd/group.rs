//! `nuee group`: a permanent group of machines, named by its own key.
//!
//! `up` reads the signed roster and either coordinates (the anchor of `serve.rs`) or joins the
//! keeper that does (`join`). The machine that created the group holds its key and is the only
//! one that can change the roster.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use iroh::RelayUrl;
use nuee_exec::RepoRule;
use nuee_net::{Net, RelayChoice};
use nuee_proto::limits::MAX_ROSTER;
use nuee_proto::{
    BoundedString, BoundedVec, DOMAIN_DELEGATE, EndpointId, GroupLink, JobLink, Link, Member,
    PROTO_VERSION, Provider, Roster, SecretKey, ServiceLink, SignedRoster,
};
use serde_json::{Value, json};

use super::serve::{self, AnchorEnd, GroupContext};
use crate::{Ctx, config, exit, fail};

/// Time one keeper has to answer.
const ASK_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a member looks for a new roster.
const POLL: Duration = Duration::from_secs(5);
/// Pause before trying again when no keeper answers.
const RETRY: Duration = Duration::from_secs(5);

/// Arguments of `nuee group`.
#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Verb,
}

#[derive(Debug, clap::Subcommand)]
enum Verb {
    /// Create a group, with this machine as its first keeper
    Create {
        /// Name of the group
        name: String,
    },
    /// Add a machine to the group (on the machine that created it)
    Invite {
        /// The group link, or `-` to read it from stdin
        link: String,
        /// The machine's id, as printed by `nuee id`
        id: String,
        /// Name shown to the group
        #[arg(long)]
        name: String,
        /// Let this machine coordinate the group
        #[arg(long)]
        keeper: bool,
    },
    /// Remove a machine from the group (on the machine that created it)
    Remove {
        /// The group link, or `-` to read it from stdin
        link: String,
        /// The machine's id
        id: String,
    },
    /// Show the group's machines
    Status {
        /// The group link, or `-` to read it from stdin
        link: String,
    },
    /// Make another keeper coordinate the group (on the machine that created it)
    Handoff {
        /// The group link, or `-` to read it from stdin
        link: String,
        /// The machine that takes over
        id: String,
    },
    /// Keep this machine in the group until Ctrl-C
    Up(UpArgs),
}

#[derive(Debug, clap::Args)]
struct UpArgs {
    /// The group link, or `-` to read it from stdin
    link: String,
    /// Tasks run at once (default: half the cores)
    #[arg(long, value_name = "N")]
    slots: Option<u16>,
    /// Accept native commands from this repository revision (repo@rev), as in `nuee join`
    #[arg(long, value_name = "REPO@REV")]
    allow_exec: Vec<String>,
    /// Run without asking the exact native commands listed in FILE, as in `nuee join`
    #[arg(long, value_name = "FILE")]
    exec_approvals: Option<PathBuf>,
    /// Let allowed native commands use the network
    #[arg(long)]
    exec_network: bool,
    /// Accept `nuee shard` work at any commit of this repository, inside the sandbox
    #[arg(long, value_name = "URL", requires = "allow_command")]
    allow_repo: Option<String>,
    /// The only shard command accepted from --allow-repo; {i} and {n} stand for numbers (repeat)
    #[arg(long, value_name = "COMMAND", requires = "allow_repo")]
    allow_command: Vec<String>,
    /// Fetch the --allow-repo URL from a local clone instead (air-gapped machines, tests)
    #[arg(long, value_name = "URL=PATH", requires = "allow_repo")]
    exec_mirror: Option<String>,
    /// Only take work when this holds: `ac` (on mains power), `idle` (the machine is not busy)
    #[arg(long, value_name = "CONDITION", value_delimiter = ',')]
    only_when: Vec<Condition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Condition {
    Ac,
    Idle,
}

/// Whether the machine runs on mains power; `true` when that cannot be told (a desktop).
fn on_mains() -> bool {
    if cfg!(target_os = "macos") {
        return std::process::Command::new("pmset")
            .args(["-g", "batt"])
            .output()
            .map_or(true, |o| {
                String::from_utf8_lossy(&o.stdout).contains("AC Power")
            });
    }
    let Ok(supplies) = std::fs::read_dir("/sys/class/power_supply") else {
        return true;
    };
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let mains: Vec<_> = supplies
        .flatten()
        .map(|e| e.path())
        .filter(|p| read(p.join("type")).trim() == "Mains")
        .collect();
    mains.is_empty() || mains.iter().any(|p| read(p.join("online")).trim() == "1")
}

fn load_average() -> Option<f64> {
    let text = if cfg!(target_os = "macos") {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).replace(['{', '}'], " ")
    } else {
        std::fs::read_to_string("/proc/loadavg").ok()?
    };
    text.split_whitespace().next()?.parse().ok()
}

/// Whether less than half the cores are busy; `true` when that cannot be told.
fn machine_idle() -> bool {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    load_average().is_none_or(|load| load < cores as f64 / 2.0)
}

/// Whether work may start now. `idle` is only checked here: once working, the machine is busy
/// because of the work itself.
fn may_start(only_when: &[Condition]) -> bool {
    only_when.iter().all(|c| match c {
        Condition::Ac => on_mains(),
        Condition::Idle => machine_idle(),
    })
}

/// Returns once the machine has been unplugged.
async fn mains_lost(only_when: &[Condition]) {
    if !only_when.contains(&Condition::Ac) {
        return std::future::pending().await;
    }
    while on_mains() {
        tokio::time::sleep(POLL).await;
    }
}

/// Runs a `nuee group` verb.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if ctx.global.ephemeral {
        return Err(fail(
            exit::USAGE,
            "`nuee group` keeps state on disk; it cannot run with --ephemeral",
        ));
    }
    match args.command {
        Verb::Create { name } => create(&name, ctx).await,
        Verb::Invite {
            link,
            id,
            name,
            keeper,
        } => invite(&link, &id, &name, keeper, ctx).await,
        Verb::Remove { link, id } => remove(&link, &id, ctx).await,
        Verb::Status { link } => status(&link, ctx).await,
        Verb::Handoff { link, id } => handoff(&link, &id, ctx).await,
        Verb::Up(a) => up(a, ctx).await,
    }
}

fn parse_link(text: &str) -> anyhow::Result<GroupLink> {
    let text = crate::link_arg(text)?;
    match Link::parse(&text).map_err(crate::link_failure)? {
        Link::Group(l) => Ok(l),
        _ => Err(fail(exit::USAGE, "this is not a group link")),
    }
}

fn parse_id(text: &str) -> anyhow::Result<EndpointId> {
    text.trim()
        .parse()
        .map_err(|_| fail(exit::USAGE, format!("{text:?} is not a machine id")))
}

fn name_of(text: &str) -> anyhow::Result<BoundedString<64>> {
    BoundedString::try_from(text)
        .map_err(|_| fail(exit::USAGE, format!("{text:?} is longer than 64 bytes")))
}

fn roster_path(ctx: &Ctx, group: &EndpointId) -> anyhow::Result<PathBuf> {
    Ok(ctx
        .data_dir()?
        .join("groups")
        .join(group.to_string())
        .join("roster.bin"))
}

fn key_path(ctx: &Ctx, group: &EndpointId) -> anyhow::Result<PathBuf> {
    Ok(config::config_dir(ctx.global.config_dir.as_deref())?
        .join("groups")
        .join(group.to_string())
        .join("group.key"))
}

fn write_in_private_dir(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        if let Some(parent) = dir.parent() {
            nuee_store::ensure_private_dir(parent).map_err(crate::store_failure)?;
        }
        nuee_store::ensure_private_dir(dir).map_err(crate::store_failure)?;
    }
    serve::write_private(path, bytes)
}

fn save_roster(ctx: &Ctx, group: &EndpointId, signed: &SignedRoster) -> anyhow::Result<()> {
    write_in_private_dir(&roster_path(ctx, group)?, &nuee_proto::encode(signed)?)
}

/// The roster kept on this machine, checked against the group key.
fn load_roster(ctx: &Ctx, group: &EndpointId) -> anyhow::Result<Option<(SignedRoster, Roster)>> {
    let path = roster_path(ctx, group)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))?;
    let signed: SignedRoster = nuee_proto::decode_with_limit(&bytes, MAX_ROSTER + 128)?;
    let roster = signed.open(group)?;
    Ok(Some((signed, roster)))
}

fn owner_key(ctx: &Ctx, group: &EndpointId) -> anyhow::Result<SecretKey> {
    let path = key_path(ctx, group)?;
    let refused = || {
        fail(
            exit::REFUSED,
            "only the machine that created the group can change its roster",
        )
    };
    let bytes = std::fs::read(&path).map_err(|_| refused())?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| refused())?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// An endpoint that may act for this machine on the coordinating keeper, and that keeper.
pub(crate) async fn member_session(
    link_text: &str,
    ctx: &Ctx,
) -> anyhow::Result<(Net, ServiceLink)> {
    let link = parse_link(link_text)?;
    let net = control_net(ctx, &link).await?;
    let (_, roster) = newest_roster(&net, &link, ctx)
        .await?
        .ok_or_else(|| fail(exit::NETWORK, "no machine of the group answered"))?;
    let leader = roster.keepers()[0].id;
    let target = service_link(&link, leader, &group_relay(&link).unwrap_or_default())?;
    let key = ctx.secret()?;
    let sig = nuee_proto::sign_domain(&key, DOMAIN_DELEGATE, net.id().as_bytes());
    let body = json!({
        "member": key.public().to_string(),
        "sig": serve::hex(&nuee_proto::encode(&sig)?),
    });
    let (status, value) = serve::admin(&net, &target, "POST", "/delegate", Some(&body)).await?;
    if status != 200 {
        return Err(serve::admin_error(status, &value));
    }
    Ok((net, target))
}

fn group_relay(link: &GroupLink) -> Option<String> {
    link.bootstrap
        .iter()
        .find_map(|p| p.relay.as_ref().map(|r| r.as_str().to_owned()))
}

/// An endpoint for reading and pushing rosters. Its key is thrown away: rosters prove themselves
/// with the group's signature, and one key on two endpoints at once would fight over the relay
/// (`group status` next to a running `group up`).
async fn control_net(ctx: &Ctx, link: &GroupLink) -> anyhow::Result<Net> {
    let key = config::fresh_key()?;
    let relay = match (&ctx.global.relay, group_relay(link)) {
        (None, Some(url)) => {
            let url: RelayUrl = url
                .parse()
                .map_err(|_| fail(exit::USAGE, "the group link names an invalid relay"))?;
            RelayChoice::Custom(vec![url])
        }
        _ => ctx.relay_choice()?,
    };
    serve::bind_online(ctx, key, relay, ctx.global.relay_only).await
}

fn service_link(link: &GroupLink, host: EndpointId, relay: &str) -> anyhow::Result<ServiceLink> {
    Ok(ServiceLink {
        host,
        relay: BoundedString::try_from(relay)
            .map_err(|_| fail(exit::USAGE, "relay URL too long"))?,
        secret: link.admin_secret(),
    })
}

/// Asks one machine for its roster; `None` when it does not answer with a valid one.
async fn ask(net: &Net, link: &GroupLink, target: &ServiceLink) -> Option<SignedRoster> {
    let reply = tokio::time::timeout(
        ASK_TIMEOUT,
        serve::admin(net, target, "GET", "/roster", None),
    );
    let (status, value) = reply.await.ok()?.ok()?;
    if status != 200 {
        return None;
    }
    let bytes = serve::unhex(value["roster"].as_str()?)?;
    let signed: SignedRoster = nuee_proto::decode_with_limit(&bytes, MAX_ROSTER + 128).ok()?;
    signed.open(&link.group).ok()?;
    Some(signed)
}

/// The newest roster among this machine's copy and the machines that answer. Keeps it on disk.
async fn newest_roster(
    net: &Net,
    link: &GroupLink,
    ctx: &Ctx,
) -> anyhow::Result<Option<(SignedRoster, Roster)>> {
    let mut best = load_roster(ctx, &link.group)?;
    let relay = group_relay(link).unwrap_or_default();
    let mut targets: Vec<EndpointId> = link.bootstrap.iter().map(|p| p.id).collect();
    if let Some((_, roster)) = &best {
        targets.extend(roster.keepers().iter().map(|k| k.id));
    }
    targets.sort_by_key(|id| *id.as_bytes());
    targets.dedup();
    let me = ctx.secret()?.public();
    let mut changed = false;
    for id in targets.into_iter().filter(|id| *id != me) {
        let Some(signed) = ask(net, link, &service_link(link, id, &relay)?).await else {
            continue;
        };
        let roster = signed.open(&link.group)?;
        if best.as_ref().is_none_or(|(_, b)| roster.replaces(b)) {
            best = Some((signed, roster));
            changed = true;
        }
    }
    if changed && let Some((signed, _)) = &best {
        save_roster(ctx, &link.group, signed)?;
    }
    Ok(best)
}

async fn create(name: &str, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let key = config::fresh_key()?;
    let group = key.public();
    let me = ctx.secret()?.public();
    let net = ctx.online_net().await?;
    let relay = Ctx::relay_string(&net);
    let roster = Roster {
        version: PROTO_VERSION,
        group,
        name: name_of(name)?,
        revision: 1,
        members: vec![Member {
            id: me,
            name: name_of("first keeper")?,
            worker: true,
            store: false,
            keeper_rank: Some(0),
        }]
        .try_into()
        .map_err(|_| anyhow::anyhow!("one member fits"))?,
    };
    let signed = roster.sign(&key)?;
    write_in_private_dir(&key_path(ctx, &group)?, &key.to_bytes())?;
    save_roster(ctx, &group, &signed)?;
    let link = Link::Group(GroupLink {
        group,
        secret: nuee_proto::new_secret16()?,
        bootstrap: vec![provider(me, &relay)?]
            .try_into()
            .map_err(|_| anyhow::anyhow!("one provider fits"))?,
    });
    ctx.ui.show_link(
        "group",
        &link.to_url(&ctx.global.web_base),
        Some(&group),
        None,
    )?;
    ctx.ui.note(&format!(
        "group {name} created; keep its key safe: {}",
        key_path(ctx, &group)?.display()
    ));
    Ok(ExitCode::SUCCESS)
}

fn provider(id: EndpointId, relay: &str) -> anyhow::Result<Provider> {
    Ok(Provider {
        id,
        relay: if relay.is_empty() {
            None
        } else {
            Some(
                BoundedString::try_from(relay)
                    .map_err(|_| fail(exit::USAGE, "relay URL too long"))?,
            )
        },
    })
}

/// Applies `change` to the next revision of the roster, tells the coordinating keeper, and
/// keeps the result unless that keeper refused it.
async fn change_roster(
    link: &GroupLink,
    ctx: &Ctx,
    change: impl FnOnce(&mut Vec<Member>) -> anyhow::Result<()>,
) -> anyhow::Result<Roster> {
    let key = owner_key(ctx, &link.group)?;
    let (_, current) = load_roster(ctx, &link.group)?
        .ok_or_else(|| fail(exit::ERROR, "this machine has no copy of the roster"))?;
    let mut members = current.members.clone().into_inner();
    change(&mut members)?;
    let next = Roster {
        revision: current.revision + 1,
        members: BoundedVec::try_from(members)
            .map_err(|_| fail(exit::USAGE, "the group is full"))?,
        ..current.clone()
    };
    let signed = next
        .sign(&key)
        .map_err(|e| fail(exit::USAGE, e.to_string()))?;

    let net = control_net(ctx, link).await?;
    let leader = current.keepers()[0].id;
    let target = service_link(link, leader, &group_relay(link).unwrap_or_default())?;
    let body = json!({ "roster": serve::hex(&nuee_proto::encode(&signed)?) });
    let reply = tokio::time::timeout(
        ASK_TIMEOUT,
        serve::admin(&net, &target, "POST", "/roster", Some(&body)),
    )
    .await;
    match reply {
        Ok(Ok((200, _))) => {}
        Ok(Ok((status, value))) => {
            return Err(fail(
                exit::REFUSED,
                format!(
                    "the coordinating keeper refused ({status}): {}",
                    value["error"].as_str().unwrap_or("no reason given")
                ),
            ));
        }
        _ => ctx.ui.note(
            "the coordinating keeper did not answer; machines will get this roster from this one",
        ),
    }
    save_roster(ctx, &link.group, &signed)?;
    Ok(next)
}

async fn invite(
    link_text: &str,
    id: &str,
    name: &str,
    keeper: bool,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    let mut link = parse_link(link_text)?;
    let id = parse_id(id)?;
    let name = name_of(name)?;
    let roster = change_roster(&link, ctx, |members| {
        if members.iter().any(|m| m.id == id) {
            return Err(fail(exit::USAGE, "this machine is already in the group"));
        }
        let next_rank = members
            .iter()
            .filter_map(|m| m.keeper_rank)
            .max()
            .map(|r| r + 1);
        members.push(Member {
            id,
            name,
            worker: true,
            store: false,
            keeper_rank: keeper.then(|| next_rank.unwrap_or(0)),
        });
        Ok(())
    })
    .await?;
    ctx.ui.emit(
        "invited",
        json!({ "id": id.to_string(), "revision": roster.revision, "keeper": keeper }),
    );
    if keeper {
        // A longer list of machines to try makes the link survive the loss of one.
        let relay = group_relay(&link).unwrap_or_default();
        if link.bootstrap.push(provider(id, &relay)?).is_ok() {
            ctx.ui.say("this link now also lists the new keeper:");
            ctx.ui.show_link(
                "group",
                &Link::Group(link.clone()).to_url(&ctx.global.web_base),
                Some(&link.group),
                None,
            )?;
        }
    }
    ctx.ui.say(&format!(
        "{id} is in the group (roster revision {})",
        roster.revision
    ));
    Ok(ExitCode::SUCCESS)
}

async fn remove(link_text: &str, id: &str, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let link = parse_link(link_text)?;
    let id = parse_id(id)?;
    let roster = change_roster(&link, ctx, |members| {
        let before = members.len();
        members.retain(|m| m.id != id);
        if members.len() == before {
            return Err(fail(exit::USAGE, "this machine is not in the group"));
        }
        Ok(())
    })
    .await?;
    ctx.ui.emit(
        "removed",
        json!({ "id": id.to_string(), "revision": roster.revision }),
    );
    ctx.ui.say(&format!(
        "{id} left the group (roster revision {})",
        roster.revision
    ));
    Ok(ExitCode::SUCCESS)
}

async fn handoff(link_text: &str, id: &str, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let link = parse_link(link_text)?;
    let id = parse_id(id)?;
    let roster = change_roster(&link, ctx, |members| {
        let target = members
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| fail(exit::USAGE, "this machine is not in the group"))?;
        target.keeper_rank = None;
        let mut others: Vec<u16> = members.iter().filter_map(|m| m.keeper_rank).collect();
        others.sort_unstable();
        for m in members.iter_mut() {
            if let Some(rank) = m.keeper_rank {
                let place = others.binary_search(&rank).unwrap_or(others.len());
                m.keeper_rank = Some(u16::try_from(place + 1).unwrap_or(u16::MAX));
            }
        }
        if let Some(target) = members.iter_mut().find(|m| m.id == id) {
            target.keeper_rank = Some(0);
        }
        Ok(())
    })
    .await?;
    ctx.ui.emit(
        "handoff",
        json!({ "id": id.to_string(), "revision": roster.revision }),
    );
    ctx.ui.say(&format!("{id} now coordinates the group"));
    Ok(ExitCode::SUCCESS)
}

fn describe(m: &Member) -> Value {
    json!({
        "id": m.id.to_string(),
        "name": m.name.as_str(),
        "keeper_rank": m.keeper_rank,
    })
}

async fn status(link_text: &str, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let link = parse_link(link_text)?;
    let net = control_net(ctx, &link).await?;
    let (_, roster) = newest_roster(&net, &link, ctx)
        .await?
        .ok_or_else(|| fail(exit::NETWORK, "no machine of the group answered"))?;
    let members: Vec<Value> = roster.members.iter().map(describe).collect();
    ctx.ui.emit(
        "group",
        json!({
            "group": link.group.to_string(),
            "name": roster.name.as_str(),
            "revision": roster.revision,
            "members": members,
        }),
    );
    if !ctx.ui.is_json() {
        ctx.ui.say(&format!(
            "{} · roster revision {} · fingerprint {}",
            roster.name,
            roster.revision,
            nuee_proto::fingerprint_string(&link.group)
        ));
        for m in roster.members.iter() {
            let role = match m.keeper_rank {
                Some(0) => "coordinates",
                Some(_) => "keeper",
                None => "member",
            };
            ctx.ui.say(&format!("{role:<12} {} {}", m.id, m.name));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Returns once a roster newer than `revision` has been kept.
async fn roster_changed(
    net: &Net,
    link: &GroupLink,
    leader: &ServiceLink,
    revision: u64,
    ctx: &Ctx,
) {
    loop {
        tokio::time::sleep(POLL).await;
        let Some(signed) = ask(net, link, leader).await else {
            continue;
        };
        let Ok(roster) = signed.open(&link.group) else {
            continue;
        };
        if roster.revision > revision {
            if let Err(e) = save_roster(ctx, &link.group, &signed) {
                tracing::warn!("cannot keep the new roster: {e:#}");
            }
            return;
        }
    }
}

fn anchor_args(up: &UpArgs) -> serve::Args {
    serve::Args {
        anchor: true,
        admin_secret_file: None,
        relay_hostname: None,
        acme_contact: None,
        relay_dev: false,
        relay_bind: std::net::SocketAddr::from(([127, 0, 0, 1], 3340)),
        relay_token_file: None,
        relay_max_conns: None,
        relay_rate: None,
        store: None,
        gateway: None,
        gateway_scripts: false,
        allow: Vec::new(),
        allow_file: None,
        approve: false,
        local_slots: up.slots.unwrap_or_else(crate::default_slots),
    }
}

fn join_args(up: &UpArgs, link: String) -> super::join::Args {
    super::join::Args {
        link,
        slots: up.slots,
        gpu: false,
        allow_exec: up.allow_exec.clone(),
        exec_approvals: up.exec_approvals.clone(),
        exec_network: up.exec_network,
        exec_mirror: up.exec_mirror.clone(),
        store: None,
        llm: None,
        repo_rules: Vec::new(),
    }
}

/// The standing shard rules asked for on the command line.
fn repo_rules(up: &UpArgs) -> anyhow::Result<Vec<RepoRule>> {
    let Some(repo) = &up.allow_repo else {
        return Ok(Vec::new());
    };
    up.allow_command
        .iter()
        .map(|c| {
            RepoRule::new(repo, c).map_err(|e| fail(exit::USAGE, format!("--allow-repo: {e}")))
        })
        .collect()
}

/// Waits, or returns `true` when the user pressed Ctrl-C meanwhile.
async fn pause(ctx: &Ctx) -> bool {
    ctx.close_nets().await;
    tokio::select! {
        () = tokio::time::sleep(RETRY) => false,
        () = crate::ctrl_c() => true,
    }
}

async fn up(args: UpArgs, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let link = parse_link(&args.link)?;
    let me = ctx.secret()?.public();
    let relay = group_relay(&link).unwrap_or_default();
    let rules = repo_rules(&args)?;
    let joining = |url: String| super::join::Args {
        repo_rules: rules.clone(),
        ..join_args(&args, url)
    };
    let native = if rules.is_empty() {
        None
    } else {
        super::join::native_runner(&joining(String::new()), &[], ctx)?
    };
    let mut held_back = false;
    ctx.ui.say(&format!(
        "group fingerprint {}  (compare with the group's other machines)",
        nuee_proto::fingerprint_string(&link.group)
    ));
    loop {
        let net = match control_net(ctx, &link).await {
            Ok(net) => net,
            Err(e) if crate::code_of(&e) == exit::NETWORK => {
                ctx.ui.note(&format!("{e:#}; trying again"));
                if pause(ctx).await {
                    return Ok(ExitCode::SUCCESS);
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        let Some((signed, roster)) = newest_roster(&net, &link, ctx).await? else {
            ctx.ui
                .note("no machine of the group answered yet; trying again");
            if pause(ctx).await {
                return Ok(ExitCode::SUCCESS);
            }
            continue;
        };
        if roster.member(&me).is_none() {
            return Err(fail(
                exit::REFUSED,
                format!(
                    "this machine is not on the group's roster; ask whoever created the group to run `nuee group invite <link> {me} --name <name>`"
                ),
            ));
        }
        let leader = roster.keepers()[0].id;
        if leader == me {
            net.close().await;
            ctx.close_nets().await;
            ctx.ui
                .say(&format!("coordinating the group {}", roster.name));
            let group_dir = roster_path(ctx, &link.group)?
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_default();
            let context = GroupContext {
                id: link.group,
                swarm_secret: link.swarm_secret(),
                admin_secret: link.admin_secret(),
                roster: signed,
                roster_path: roster_path(ctx, &link.group)?,
                dir: group_dir.join("anchor"),
                relay: relay.parse().ok(),
                native: native.clone(),
            };
            match serve::run_anchor(anchor_args(&args), ctx, Some(context)).await? {
                AnchorEnd::Stopped => return Ok(ExitCode::SUCCESS),
                AnchorEnd::HandedOff => {
                    ctx.close_nets().await;
                    continue;
                }
            }
        }

        if !may_start(&args.only_when) {
            if !held_back {
                ctx.ui.note("waiting for the conditions of --only-when");
                held_back = true;
            }
            if pause(ctx).await {
                return Ok(ExitCode::SUCCESS);
            }
            continue;
        }
        held_back = false;

        let job = Link::Job(JobLink {
            coordinator: leader,
            relay: BoundedString::try_from(relay.as_str())
                .map_err(|_| fail(exit::USAGE, "relay URL too long"))?,
            secret: link.swarm_secret(),
        })
        .to_url(&ctx.global.web_base);
        let target = service_link(&link, leader, &relay)?;
        let joined = super::join::run(joining(job), ctx);
        let changed = roster_changed(&net, &link, &target, roster.revision, ctx);
        tokio::pin!(joined);
        tokio::pin!(changed);
        tokio::select! {
            result = &mut joined => match result {
                Ok(_) => {}
                Err(e) if [exit::NETWORK, exit::ENDED].contains(&crate::code_of(&e)) => {
                    ctx.ui.note(&format!("{e:#}; trying again"));
                }
                Err(e) => return Err(e),
            },
            () = &mut changed => ctx.ui.note("the group's roster changed"),
            () = mains_lost(&args.only_when) => ctx.ui.note("unplugged: leaving until --only-when holds"),
            () = crate::ctrl_c() => return Ok(ExitCode::SUCCESS),
        }
        if pause(ctx).await {
            return Ok(ExitCode::SUCCESS);
        }
    }
}
