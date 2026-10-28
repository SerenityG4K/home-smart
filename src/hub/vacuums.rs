//! Aspirateurs robots dans le hub.
//!
//! - Robots Tuya : des appareils comme les autres (acteur Tuya, garde de la liaison) ; leurs DP sont
//!   traduits par `crate::vacuum::tuya`.
//! - Robots Valetudo et miIO : un acteur par robot, qui relit son état (toutes les 3 s en
//!   mouvement, 15 s sinon) et exécute les commandes.
//!
//! Pour tous : une vue commune (`VacuumView`), des commandes inscrites au journal avec leur origine
//! (toi, ligne de commande, programme, scénario), les nettoyages lancés ou terminés sans HomeSmart
//! inscrits au journal (source « Appareil »), et les erreurs du robot confiées à la détection des
//! problèmes (« Aspirateur en difficulté », avec la gravité et ce qu'il faut faire).

use super::{Event, Hub, Level, Source};
use crate::automation::Action;
use crate::discovery::now_ms;
use crate::kinds::{self, Kind};
use crate::problems::{self, OfflineInfo, ProblemKind};
use crate::vacuum::{miio, tuya, valetudo::Valetudo, Activity, Caps, Command, Family, Link, Status, VacuumConfig};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep_until;

/// Une commande de HomeSmart compte comme la cause d'un changement d'activité pendant ce délai.
const OURS_FOR: Duration = Duration::from_secs(90);

pub(super) enum VacCmd {
    Run(Command, oneshot::Sender<Result<()>>),
    Refresh,
    Shutdown,
}

#[derive(Clone, Default)]
struct VacState {
    online: bool,
    status: Status,
    caps: Caps,
    error: Option<String>,
    updated: u64,
    /// Liaison perdue (ou pas encore établie) depuis.
    lost_at: Option<Instant>,
}

/// Robot hors Tuya (Valetudo, miIO).
pub(super) struct VacuumHandle {
    cfg: RwLock<VacuumConfig>,
    state: RwLock<VacState>,
    tx: mpsc::Sender<VacCmd>,
}

impl VacuumHandle {
    fn name(&self) -> String {
        self.cfg.read().unwrap().display_name().to_string()
    }
}

/// Suivi commun à tous les robots (Tuya ou non).
#[derive(Default)]
pub(super) struct VacWatch {
    /// Activité au relevé précédent.
    last: Option<Activity>,
    /// Dernière commande envoyée par HomeSmart, et quand.
    ours: Option<(Command, Instant)>,
}

/// Un aspirateur robot, quelle que soit sa famille.
#[derive(Serialize, Clone, Debug)]
pub struct VacuumView {
    pub id: String,
    pub name: String,
    pub room: Option<String>,
    pub family: Family,
    pub host: Option<String>,
    pub model: Option<String>,
    pub online: bool,
    pub error: Option<String>,
    pub status: Status,
    pub caps: Caps,
    pub updated: u64,
}

impl VacuumView {
    fn of_tuya(d: &super::DeviceView) -> VacuumView {
        VacuumView {
            id: d.id.clone(),
            name: d.name.clone(),
            room: d.room.clone(),
            family: Family::Tuya,
            host: d.ip.clone(),
            model: d.version.as_ref().map(|v| format!("Tuya, protocole {v}")),
            online: d.online,
            error: d.error.clone(),
            status: tuya::status(&d.dps, &d.schema),
            caps: tuya::caps(&d.dps, &d.schema),
            updated: d.updated,
        }
    }

    fn of(h: &VacuumHandle) -> VacuumView {
        let cfg = h.cfg.read().unwrap().clone();
        let st = h.state.read().unwrap().clone();
        VacuumView {
            name: cfg.display_name().to_string(),
            id: cfg.id,
            room: cfg.room,
            family: cfg.link.family(),
            host: Some(cfg.link.host().to_string()),
            model: cfg.model,
            online: st.online,
            error: st.error,
            status: st.status,
            caps: st.caps,
            updated: st.updated,
        }
    }
}

/// Modification d'un robot hors Tuya (`None` : inchangé ; texte vide : effacé).
#[derive(Default)]
pub struct VacuumPatch {
    pub name: Option<String>,
    pub room: Option<String>,
    pub link: Option<Link>,
}

/// Pilote d'un robot hors Tuya.
enum Driver {
    Valetudo(Arc<Mutex<Valetudo>>),
    Miio(miio::Robot),
}

impl Driver {
    /// Connexion ; renvoie aussi le modèle annoncé.
    async fn connect(link: &Link) -> Result<(Driver, String)> {
        match link {
            Link::Valetudo { host, port, user, password } => {
                let mut v = Valetudo::new(host, *port, user.as_deref(), password.as_deref());
                let (v, info) = tokio::task::spawn_blocking(move || v.connect().map(|i| (v, i))).await??;
                Ok((Driver::Valetudo(Arc::new(Mutex::new(v))), info.label()))
            }
            Link::Miio { host, token, port } => {
                let r = miio::Robot::connect(host, *port, token).await?;
                let model = r.model.clone();
                Ok((Driver::Miio(r), model))
            }
        }
    }

    async fn read(&mut self) -> Result<(Status, Caps)> {
        match self {
            Driver::Valetudo(v) => {
                let v = v.clone();
                tokio::task::spawn_blocking(move || {
                    let v = v.lock().unwrap();
                    v.read().map(|s| (s, v.caps.clone()))
                })
                .await?
            }
            Driver::Miio(r) => {
                let s = r.read().await?;
                Ok((s, r.caps.clone()))
            }
        }
    }

    async fn run(&mut self, cmd: &Command) -> Result<()> {
        match self {
            Driver::Valetudo(v) => {
                let (v, cmd) = (v.clone(), cmd.clone());
                tokio::task::spawn_blocking(move || v.lock().unwrap().run(&cmd)).await?
            }
            Driver::Miio(r) => r.run(cmd).await,
        }
    }
}

/// Essai de connexion (fenêtre d'ajout, ligne de commande) : modèle et état.
pub async fn test_link(link: &Link) -> Result<(String, Status)> {
    let (model, st, _) = read_link(link).await?;
    Ok((model, st))
}

/// Lecture directe (ligne de commande) : modèle, état et ce que le robot sait faire.
pub async fn read_link(link: &Link) -> Result<(String, Status, Caps)> {
    let (mut d, model) = Driver::connect(link).await?;
    let (st, caps) = d.read().await?;
    Ok((model, st, caps))
}

/// Commande directe (ligne de commande) : connexion, vérification, exécution. Renvoie la
/// description de la commande (pour le journal) et son résultat ; l'erreur extérieure dit que le
/// robot est injoignable.
pub async fn run_link(link: &Link, cmd: &Command) -> Result<(String, Result<()>)> {
    let (mut d, _) = Driver::connect(link).await?;
    let (_, caps) = d.read().await?;
    let what = cmd.describe(&caps);
    if let Err(e) = cmd.check(&caps) {
        return Ok((what, Err(anyhow!(e))));
    }
    let r = d.run(cmd).await;
    Ok((what, r))
}

impl Hub {
    // ---- Lecture -----------------------------------------------------------------------------

    fn vacuum_handle(&self, id: &str) -> Option<Arc<VacuumHandle>> {
        self.vacuums.read().unwrap().get(id).cloned()
    }

    pub(super) fn vacuum_name(&self, id: &str) -> Option<String> {
        self.vacuum_handle(id).map(|h| h.name())
    }

    /// Appareil Tuya reconnu comme aspirateur robot ?
    fn is_tuya_vacuum(&self, id: &str) -> bool {
        self.handle(id).is_some_and(|h| {
            let cfg = h.cfg.read().unwrap();
            kinds::detect(cfg.kind.as_deref(), &h.state.read().unwrap().dps) == Kind::Vacuum
        })
    }

    /// Aspirateur robot (Tuya ou non) ?
    pub fn is_vacuum(&self, id: &str) -> bool {
        self.vacuum_handle(id).is_some() || self.is_tuya_vacuum(id)
    }

    /// Tous les aspirateurs robots : les Tuya (dans l'ordre des appareils), puis les autres.
    pub fn vacuums(&self) -> Vec<VacuumView> {
        let mut out: Vec<VacuumView> = self.list().iter().filter(|d| kinds::detect(d.kind.as_deref(), &d.dps) == Kind::Vacuum).map(VacuumView::of_tuya).collect();
        let order: Vec<String> = self.config.lock().unwrap().vacuums.iter().map(|v| v.id.clone()).collect();
        let handles = self.vacuums.read().unwrap();
        out.extend(order.iter().filter_map(|id| handles.get(id)).map(|h| VacuumView::of(h)));
        out
    }

    /// Configuration complète (jeton, mot de passe compris) d'un robot hors Tuya.
    pub fn vacuum_config(&self, id: &str) -> Option<VacuumConfig> {
        self.vacuum_handle(id).map(|h| h.cfg.read().unwrap().clone())
    }

    pub fn vacuum(&self, id: &str) -> Option<VacuumView> {
        if let Some(h) = self.vacuum_handle(id) {
            return Some(VacuumView::of(&h));
        }
        self.view(id).filter(|d| kinds::detect(d.kind.as_deref(), &d.dps) == Kind::Vacuum).map(|d| VacuumView::of_tuya(&d))
    }

    /// Identifiant d'un aspirateur d'après son identifiant ou son nom.
    pub fn resolve_vacuum(&self, query: &str) -> Option<String> {
        let list = self.vacuums();
        let q = query.trim();
        let ql = q.to_lowercase();
        list.iter()
            .find(|v| v.id == q || v.host.as_deref() == Some(q))
            .or_else(|| list.iter().find(|v| v.name.eq_ignore_ascii_case(q)))
            .or_else(|| {
                let mut hits = list.iter().filter(|v| v.name.to_lowercase().contains(&ql));
                match (hits.next(), hits.next()) {
                    (Some(v), None) => Some(v),
                    _ => None,
                }
            })
            .map(|v| v.id.clone())
    }

    // ---- Commandes ---------------------------------------------------------------------------

    /// Commande un robot. Inscrite au journal avec son origine, réussie, refusée ou échouée.
    pub async fn vacuum_command(&self, id: &str, cmd: Command, source: Source) -> Result<()> {
        let view = self.vacuum(id).ok_or_else(|| anyhow!("aspirateur inconnu"))?;
        let what = cmd.describe(&view.caps);
        if let Err(e) = cmd.check(&view.caps) {
            self.log(Level::Warning, source, id, format!("Refusé : {what} — {e}"));
            bail!(e);
        }
        if !view.online {
            self.log(Level::Warning, source, id, format!("Échec : {what} (robot hors ligne)"));
            bail!("robot hors ligne");
        }
        // Noté avant l'envoi : le robot peut changer d'activité avant la fin de cette fonction.
        self.vac_watch.lock().unwrap().entry(id.to_string()).or_default().ours = Some((cmd.clone(), Instant::now()));
        if view.family == Family::Tuya {
            let (dps, schema) = {
                let h = self.handle(id).ok_or_else(|| anyhow!("aspirateur inconnu"))?;
                let dps = h.state.read().unwrap().dps.clone();
                let schema = h.cfg.read().unwrap().schema.clone();
                (dps, schema)
            };
            let m = match tuya::command(&cmd, &dps, &schema) {
                Ok(m) => m,
                Err(e) => {
                    self.log(Level::Warning, source, id, format!("Refusé : {what} — {e}"));
                    bail!(e);
                }
            };
            // `set_dps_as` inscrit la commande au journal (ou son échec).
            return self.set_dps_as(id, m, source, Some(what)).await;
        }
        let h = self.vacuum_handle(id).ok_or_else(|| anyhow!("aspirateur inconnu"))?;
        let r = async {
            let (tx, rx) = oneshot::channel();
            h.tx.send(VacCmd::Run(cmd, tx)).await.map_err(|_| anyhow!("robot arrêté"))?;
            tokio::time::timeout(Duration::from_secs(15), rx).await.map_err(|_| anyhow!("délai dépassé"))?.map_err(|_| anyhow!("robot arrêté"))?
        }
        .await;
        match &r {
            Ok(()) => self.log(Level::Info, source, id, what),
            Err(e) => self.log(Level::Warning, source, id, format!("Échec : {what} ({e:#})")),
        }
        r
    }

    /// Action d'un programme ou d'un scénario.
    pub async fn run_action(&self, device: &str, action: Action, source: Source) -> Result<()> {
        match action {
            Action::On | Action::Off => self.power(device, action == Action::On, None, source).await.map(|_| ()),
            Action::Clean => self.vacuum_command(device, Command::Start, source).await,
            Action::Dock => self.vacuum_command(device, Command::Dock, source).await,
        }
    }

    pub fn vacuum_refresh(&self, id: &str) {
        if let Some(h) = self.vacuum_handle(id) {
            let _ = h.tx.try_send(VacCmd::Refresh);
        } else {
            let _ = self.refresh(id);
        }
    }

    // ---- Ajout, réglages, suppression --------------------------------------------------------

    pub fn add_vacuum(self: &Arc<Self>, mut cfg: VacuumConfig, source: Source) -> Result<VacuumView> {
        let host = cfg.link.host().trim().to_string();
        if host.is_empty() {
            bail!("adresse IP manquante");
        }
        if let Link::Miio { token, .. } = &cfg.link {
            miio::parse_token(token)?;
        }
        {
            let config = self.config.lock().unwrap();
            if let Some(v) = config.vacuums.iter().find(|v| v.link.host() == host && v.link.port() == cfg.link.port()) {
                bail!("cet aspirateur est déjà ajouté (« {} »)", v.display_name());
            }
            if cfg.id.trim().is_empty() {
                // Identifiant tiré de l'adresse ; numéroté si deux robots la partagent (essais).
                let base = VacuumConfig::make_id(&host);
                let taken = |id: &str| config.vacuums.iter().any(|v| v.id == id) || config.devices.iter().any(|d| d.id == id);
                cfg.id = (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") }).find(|id| !taken(id)).unwrap_or(base);
            } else if config.vacuums.iter().any(|v| v.id == cfg.id) {
                bail!("identifiant déjà pris : {}", cfg.id);
            }
        }
        if cfg.name.trim().is_empty() {
            cfg.name = "Aspirateur".into();
        }
        self.config.lock().unwrap().vacuums.push(cfg.clone());
        let h = self.spawn_vacuum(cfg.clone());
        self.log(Level::Info, source, &cfg.id, format!("Aspirateur ajouté ({}, {host})", cfg.link.family().label()));
        self.persist();
        self.emit(Event::Device(cfg.id.clone()));
        Ok(VacuumView::of(&h))
    }

    pub fn update_vacuum(self: &Arc<Self>, id: &str, p: VacuumPatch, source: Source) -> Result<VacuumView> {
        let h = self.vacuum_handle(id).ok_or_else(|| anyhow!("aspirateur inconnu"))?;
        let before = h.cfg.read().unwrap().clone();
        let mut c = before.clone();
        if let Some(n) = p.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
            c.name = n;
        }
        if let Some(r) = p.room {
            c.room = Some(r.trim().to_string()).filter(|r| !r.is_empty());
        }
        if let Some(l) = p.link {
            if let Link::Miio { token, .. } = &l {
                miio::parse_token(token)?;
            }
            c.link = l;
        }
        let mut changes = Vec::new();
        if c.name != before.name {
            changes.push(format!("renommé « {} » → « {} »", before.display_name(), c.display_name()));
        }
        if c.room != before.room {
            changes.push(format!("pièce : {}", c.room.as_deref().unwrap_or("aucune")));
        }
        if c.link != before.link {
            // Ni le jeton ni le mot de passe ne vont au journal.
            changes.push(format!("connexion : {} ({})", c.link.family().label(), c.link.host()));
        }
        if changes.is_empty() {
            return Ok(VacuumView::of(&h));
        }
        let reconnect = c.link != before.link;
        *h.cfg.write().unwrap() = c.clone();
        if let Some(x) = self.config.lock().unwrap().vacuums.iter_mut().find(|v| v.id == id) {
            *x = c.clone();
        }
        self.log(Level::Info, source, id, format!("Réglages : {}", changes.join(", ")));
        self.persist();
        let h = if reconnect {
            let _ = h.tx.try_send(VacCmd::Shutdown);
            self.spawn_vacuum(c)
        } else {
            h
        };
        self.emit(Event::Device(id.to_string()));
        Ok(VacuumView::of(&h))
    }

    pub fn remove_vacuum(&self, id: &str, source: Source) -> Result<()> {
        let h = self.vacuums.write().unwrap().remove(id).ok_or_else(|| anyhow!("aspirateur inconnu"))?;
        self.record(crate::journal::Entry::new(Level::Info, source, id, &h.name(), "Aspirateur supprimé (ses programmes et scénarios aussi)"));
        let _ = h.tx.try_send(VacCmd::Shutdown);
        {
            let mut cfg = self.config.lock().unwrap();
            cfg.vacuums.retain(|v| v.id != id);
            cfg.schedules.retain(|s| s.device != id);
            cfg.scenarios.retain(|s| s.device != id);
        }
        self.vac_watch.lock().unwrap().remove(id);
        self.forget_problems(id);
        self.persist();
        self.emit(Event::Removed(id.to_string()));
        self.emit(Event::Schedules);
        Ok(())
    }

    // ---- Acteurs ----------------------------------------------------------------------------------

    pub(super) fn start_vacuums(self: &Arc<Self>) {
        let list = self.config.lock().unwrap().vacuums.clone();
        for cfg in list {
            self.spawn_vacuum(cfg);
        }
    }

    fn spawn_vacuum(self: &Arc<Self>, cfg: VacuumConfig) -> Arc<VacuumHandle> {
        let (tx, rx) = mpsc::channel(16);
        let id = cfg.id.clone();
        let h = Arc::new(VacuumHandle { cfg: RwLock::new(cfg), state: RwLock::new(VacState { lost_at: Some(Instant::now()), ..Default::default() }), tx });
        if let Some(old) = self.vacuums.write().unwrap().insert(id, h.clone()) {
            let _ = old.tx.try_send(VacCmd::Shutdown);
        }
        tokio::spawn(run_vacuum(self.clone(), h.clone(), rx));
        h
    }

    // ---- Suivi : journal et problèmes ----------------------------------------------------------------

    /// Après chaque relevé d'un robot (Tuya ou non) : changements d'activité au journal, erreurs aux
    /// problèmes.
    pub(super) fn vacuum_observe(&self, id: &str, st: &Status) {
        let (prev, ours) = {
            let mut w = self.vac_watch.lock().unwrap();
            let w = w.entry(id.to_string()).or_default();
            let prev = w.last.replace(st.activity);
            let ours = w.ours.as_ref().filter(|(_, t)| t.elapsed() < OURS_FOR).map(|(c, _)| c.clone());
            (prev, ours)
        };
        if let Some(prev) = prev.filter(|p| *p != st.activity && *p != Activity::Unknown) {
            let cleaning = |a: Activity| matches!(a, Activity::Cleaning | Activity::Paused);
            if st.activity == Activity::Cleaning && !cleaning(prev) && !matches!(ours, Some(Command::Start | Command::Pattern(_))) {
                self.log(Level::Info, Source::Device, id, "Nettoyage lancé hors de HomeSmart (bouton du robot, son application ou son programme interne)");
            } else if cleaning(prev) && matches!(st.activity, Activity::Returning | Activity::Charging | Activity::Docked | Activity::Idle | Activity::Station) {
                let mut details = Vec::new();
                if let Some(a) = st.area_m2.filter(|a| *a > 0.0) {
                    details.push(format!("{} m²", kinds::fr(a, if a < 10.0 { 1 } else { 0 })));
                }
                if let Some(m) = st.clean_min.filter(|m| *m > 0) {
                    details.push(format!("en {}", kinds::fmt_secs(m as u64 * 60)));
                }
                let why = match (st.activity, &ours) {
                    (_, Some(Command::Stop)) => " (arrêté par HomeSmart)",
                    (_, Some(Command::Dock)) => " (retour demandé par HomeSmart)",
                    (Activity::Returning, _) if st.battery.is_some_and(|b| b <= 25) => " : batterie faible, retour à la base pour recharger",
                    (Activity::Returning | Activity::Charging | Activity::Docked, _) => ", retour à la base",
                    _ => "",
                };
                let details = if details.is_empty() { String::new() } else { format!(" : {}", details.join(" ")) };
                self.log(Level::Info, Source::Device, id, format!("Nettoyage terminé{details}{why}"));
            } else if prev == Activity::Returning && st.activity.at_dock() {
                self.log(Level::Info, Source::Device, id, "Revenu sur sa base");
            }
        }
        if st.errors.is_empty() {
            if self.problem_open(ProblemKind::VacuumError, id) {
                self.settle(ProblemKind::VacuumError, id, "le robot ne signale plus d'erreur");
            }
        } else {
            let (level, text) = problems::vacuum_problem(&st.errors);
            self.raise(ProblemKind::VacuumError, id, level, text);
        }
    }

    /// Toutes les 5 s : robots hors Tuya injoignables depuis trop longtemps.
    pub(super) fn vacuum_problems_tick(&self, offline_after: Duration) {
        let handles: Vec<Arc<VacuumHandle>> = self.vacuums.read().unwrap().values().cloned().collect();
        for h in handles {
            let (id, info) = {
                let st = h.state.read().unwrap();
                let Some(t) = st.lost_at.filter(|t| !st.online && t.elapsed() >= offline_after) else { continue };
                let info = OfflineInfo { for_: t.elapsed(), error: st.error.clone().unwrap_or_default(), robot: true, ..Default::default() };
                (h.cfg.read().unwrap().id.clone(), info)
            };
            let (level, text) = problems::offline_problem(&info);
            self.raise(ProblemKind::Offline, &id, level, text);
        }
    }
}

/// Rythme des relevés : plus serré quand le robot bouge.
fn poll_every(a: Activity) -> Duration {
    match a {
        Activity::Cleaning | Activity::Returning => Duration::from_secs(3),
        Activity::Error | Activity::Paused => Duration::from_secs(8),
        _ => Duration::from_secs(15),
    }
}

async fn run_vacuum(hub: Arc<Hub>, h: Arc<VacuumHandle>, mut rx: mpsc::Receiver<VacCmd>) {
    let mut backoff = Duration::from_secs(2);
    loop {
        let mut connected = false;
        let result = session(&hub, &h, &mut rx, &mut connected).await;
        let err = match result {
            Ok(()) => return, // arrêt demandé
            Err(e) => format!("{e:#}"),
        };
        let id = h.cfg.read().unwrap().id.clone();
        let (changed, was_online) = {
            let mut s = h.state.write().unwrap();
            let changed = s.online || s.error.as_deref() != Some(err.as_str());
            let was_online = s.online;
            s.online = false;
            s.error = Some(err.clone());
            if was_online {
                s.lost_at = Some(Instant::now());
            }
            s.lost_at.get_or_insert_with(Instant::now);
            (changed, was_online)
        };
        if changed {
            let what = if was_online { "Connexion perdue" } else { "Injoignable" };
            hub.log(Level::Info, Source::System, &id, format!("{what} : {err}"));
            hub.emit(Event::Device(id.clone()));
        }
        if connected {
            backoff = Duration::from_secs(2);
        }
        let until = tokio::time::Instant::now() + backoff;
        loop {
            tokio::select! {
                _ = sleep_until(until) => break,
                c = rx.recv() => match c {
                    None | Some(VacCmd::Shutdown) => return,
                    Some(VacCmd::Run(_, reply)) => { let _ = reply.send(Err(anyhow!("robot hors ligne : {err}"))); }
                    Some(VacCmd::Refresh) => break,
                }
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn session(hub: &Arc<Hub>, h: &Arc<VacuumHandle>, rx: &mut mpsc::Receiver<VacCmd>, connected: &mut bool) -> Result<()> {
    let cfg = h.cfg.read().unwrap().clone();
    let (mut drv, model) = Driver::connect(&cfg.link).await?;
    // En ligne seulement avec un premier état complet (et ce que le robot sait faire).
    let (status, caps) = drv.read().await?;
    *connected = true;
    let id = cfg.id.clone();
    let outage = {
        let mut s = h.state.write().unwrap();
        (s.online, s.error, s.status, s.caps, s.updated) = (true, None, status.clone(), caps, now_ms());
        s.lost_at.take().map(|t| t.elapsed())
    };
    if !model.is_empty() && cfg.model.as_deref() != Some(model.as_str()) {
        h.cfg.write().unwrap().model = Some(model.clone());
        if let Some(x) = hub.config.lock().unwrap().vacuums.iter_mut().find(|v| v.id == id) {
            x.model = Some(model.clone());
        }
        hub.persist();
    }
    if let Some(d) = outage.filter(|_| hub.problem_open(ProblemKind::Offline, &id)) {
        hub.settle(ProblemKind::Offline, &id, &format!("de nouveau en ligne (hors ligne {})", kinds::fmt_secs(d.as_secs())));
    }
    let model = if model.is_empty() { String::new() } else { format!("{model}, ") };
    hub.log(Level::Info, Source::System, &id, format!("Connecté ({}{}, {})", model, cfg.link.family().label(), cfg.link.host()));
    hub.vacuum_observe(&id, &status);
    hub.emit(Event::Device(id.clone()));
    let mut next = tokio::time::Instant::now() + poll_every(status.activity);
    loop {
        tokio::select! {
            _ = sleep_until(next) => {
                let (status, caps) = drv.read().await?;
                {
                    let mut s = h.state.write().unwrap();
                    (s.status, s.caps, s.updated) = (status.clone(), caps, now_ms());
                }
                hub.vacuum_observe(&id, &status);
                hub.emit(Event::Device(id.clone()));
                next = tokio::time::Instant::now() + poll_every(status.activity);
            }
            cmd = rx.recv() => match cmd {
                None | Some(VacCmd::Shutdown) => return Ok(()),
                Some(VacCmd::Run(c, reply)) => {
                    let r = drv.run(&c).await;
                    let _ = reply.send(r);
                    // Relire tout de suite l'effet de la commande.
                    next = tokio::time::Instant::now() + Duration::from_millis(1200);
                }
                Some(VacCmd::Refresh) => next = tokio::time::Instant::now(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::{Schedule, Scenario, Trigger};
    use crate::config::DeviceConfig;
    use crate::protocol::Version;
    use crate::vacuum::{sim, ErrorKind};
    use serde_json::Value;

    async fn until(hub: &Hub, secs: u64, what: &str, pred: impl Fn(&Hub) -> bool) {
        let t0 = Instant::now();
        while !pred(hub) {
            let log: Vec<String> = hub.alerts(30).iter().map(|a| format!("{} [{:?}] {}", a.device_name, a.source, a.message)).collect();
            assert!(t0.elapsed() < Duration::from_secs(secs), "{what} : délai dépassé ; journal : {log:#?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn logged(hub: &Hub, source: Source, text: &str) -> bool {
        hub.alerts(300).iter().any(|a| a.source == source && a.message.contains(text))
    }

    /// Robot Tuya simulé (description standard) : commandes, fin de nettoyage, erreur → problème,
    /// programme « nettoyer », scénario « mode absence → nettoyer ».
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn tuya_vacuum_end_to_end() {
        let dir = std::env::temp_dir().join(format!("homesmart-vacuum-{}", rand::random::<u32>()));
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.start(false);
        let id = "bf0000000000vacuum01";
        let key = *b"0123456789abcdef";
        let spec = crate::sim::SimSpec { id: id.into(), key, version: Version::V34, dps: tuya::standard_dps(), watts: 0 };
        let port = crate::sim::spawn(spec, "127.0.0.1:0").await.unwrap();
        hub.add(DeviceConfig {
            id: id.into(),
            key: "0123456789abcdef".into(),
            name: "Robot".into(),
            ip: Some("127.0.0.1".into()),
            port: Some(port),
            version: Some("3.4".into()),
            kind: Some("vacuum".into()),
            schema: tuya::standard_schema().as_object().cloned().unwrap(),
            ..Default::default()
        })
        .unwrap();
        until(&hub, 10, "connexion", |h| h.vacuum(id).is_some_and(|v| v.online && v.status.battery == Some(100))).await;
        assert!(hub.is_vacuum(id));
        assert_eq!(hub.vacuums().len(), 1);
        let brain = sim::robot(id).unwrap();
        brain.lock().unwrap().speed = 20.0;

        // Lancer : une seule entrée, avec toi comme origine ; pas de « lancé hors de HomeSmart ».
        hub.vacuum_command(id, Command::Start, Source::User).await.unwrap();
        until(&hub, 8, "nettoyage", |h| h.vacuum(id).is_some_and(|v| v.status.activity == Activity::Cleaning)).await;
        hub.vacuum_command(id, Command::Fan("max".into()), Source::User).await.unwrap();
        until(&hub, 8, "puissance", |h| h.vacuum(id).is_some_and(|v| v.status.fan.as_deref() == Some("max"))).await;
        assert!(logged(&hub, Source::User, "Nettoyage lancé"));
        assert!(logged(&hub, Source::User, "Puissance d'aspiration : Maximum"));
        assert!(!logged(&hub, Source::Device, "hors de HomeSmart"));
        // Puissance inconnue : refusée et inscrite.
        assert!(hub.vacuum_command(id, Command::Fan("turbo".into()), Source::User).await.is_err());
        assert!(logged(&hub, Source::User, "Refusé : Puissance d'aspiration"));

        // Retour à la base demandé : fin du nettoyage inscrite, avec la surface.
        hub.vacuum_command(id, Command::Dock, Source::Cli).await.unwrap();
        until(&hub, 15, "retour", |h| h.vacuum(id).is_some_and(|v| v.status.activity.at_dock())).await;
        assert!(logged(&hub, Source::Device, "Nettoyage terminé"), "{:#?}", hub.alerts(20));
        assert!(logged(&hub, Source::Cli, "Retour à la base demandé"));

        // Lancé par le bouton du robot : inscrit comme venant de l'appareil.
        brain.lock().unwrap().command(&Command::Start).unwrap();
        until(&hub, 10, "lancé ailleurs", |h| logged(h, Source::Device, "Nettoyage lancé hors de HomeSmart")).await;

        // Brosse bloquée : problème (avertissement, avec le conseil), puis résolu.
        brain.lock().unwrap().set_error(Some(ErrorKind::MainBrush));
        until(&hub, 10, "erreur", |h| h.problems_of(id).iter().any(|p| p.kind == ProblemKind::VacuumError)).await;
        let p = hub.problems_of(id).into_iter().find(|p| p.kind == ProblemKind::VacuumError).unwrap();
        assert_eq!(p.level, Level::Warning);
        assert!(p.detail.starts_with("le robot signale « Brosse principale bloquée » : retire les cheveux"), "{}", p.detail);
        brain.lock().unwrap().set_error(None);
        until(&hub, 10, "résolu", |h| h.problems_of(id).is_empty()).await;
        assert!(logged(&hub, Source::Device, "Résolu — Aspirateur en difficulté"));

        // Programme « nettoyer » : accepté pour un robot, refusé pour « allumer » une action robot sur autre chose.
        let s = Schedule { id: 0, device: id.into(), enabled: true, days: [true; 7], start: "10:00".into(), end: None, action: Action::Clean };
        hub.save_schedule(s).unwrap();
        assert!(logged(&hub, Source::User, "Programme créé : Nettoyer à 10:00, tous les jours"));

        // Scénario de la maison entière : mode absence activé → nettoyer.
        brain.lock().unwrap().set_error(None);
        hub.vacuum_command(id, Command::Dock, Source::User).await.unwrap();
        until(&hub, 15, "à la base", |h| h.vacuum(id).is_some_and(|v| v.status.activity.at_dock())).await;
        let sc = Scenario { id: 0, enabled: true, camera: String::new(), trigger: Trigger::AwayOn, device: id.into(), action: Action::Clean, during: None };
        hub.add_scenario(sc, Source::User).unwrap();
        hub.set_vision_armed(true, Source::User);
        until(&hub, 10, "scénario", |h| h.vacuum(id).is_some_and(|v| v.status.activity == Activity::Cleaning)).await;
        assert!(logged(&hub, Source::Scenario, "Scénario — Maison : mode absence activé → nettoyer avec « Robot »"), "{:#?}", hub.alerts(20));
        assert!(logged(&hub, Source::Scenario, "Nettoyage lancé"));
        hub.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Robots hors Tuya : Valetudo et miIO simulés, ajoutés, pilotés, perdus puis supprimés.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn valetudo_and_miio_vacuums() {
        let dir = std::env::temp_dir().join(format!("homesmart-vacuum2-{}", rand::random::<u32>()));
        let hub = Hub::new(dir.join("homesmart.json")).unwrap();
        hub.set_fast_detection(true);
        hub.start(false);
        let (vport, vrobot) = sim::start_valetudo("L10s Ultra", None).await.unwrap();
        let link = Link::Valetudo { host: "127.0.0.1".into(), port: Some(vport), user: None, password: None };
        let (model, st) = test_link(&link).await.unwrap();
        assert_eq!((model.as_str(), st.activity), ("Dreame L10s Ultra", Activity::Docked));
        let v = hub.add_vacuum(VacuumConfig { id: String::new(), name: "Étage".into(), room: Some("Chambre".into()), link, model: None }, Source::User).unwrap();
        assert!(hub.add_vacuum(VacuumConfig { id: String::new(), name: "Double".into(), room: None, link: Link::Valetudo { host: "127.0.0.1".into(), port: Some(vport), user: None, password: None }, model: None }, Source::User).is_err());
        until(&hub, 10, "valetudo en ligne", |h| h.vacuum(&v.id).is_some_and(|x| x.online && !x.caps.fans.is_empty())).await;
        assert_eq!(hub.vacuum(&v.id).unwrap().model.as_deref(), Some("Dreame L10s Ultra"));
        hub.vacuum_command(&v.id, Command::Mode("mop".into()), Source::User).await.unwrap();
        hub.vacuum_command(&v.id, Command::Start, Source::User).await.unwrap();
        until(&hub, 8, "valetudo nettoie", |h| h.vacuum(&v.id).is_some_and(|x| x.status.activity == Activity::Cleaning && x.status.mode.as_deref() == Some("mop"))).await;
        assert!(logged(&hub, Source::User, "Mode de nettoyage : Serpillière"));
        assert_eq!(hub.resolve_vacuum("étage"), Some(v.id.clone()));
        vrobot.lock().unwrap().set_error(Some(ErrorKind::Lifted));
        until(&hub, 12, "valetudo erreur", |h| h.problems_of(&v.id).iter().any(|p| p.kind == ProblemKind::VacuumError)).await;

        let token = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
        let msim = sim::start_miio("roborock.vacuum.s5", miio::parse_token(token).unwrap()).await.unwrap();
        let m = hub.add_vacuum(VacuumConfig { id: String::new(), name: "Roborock".into(), room: None, link: Link::Miio { host: "127.0.0.1".into(), token: token.into(), port: Some(msim.port) }, model: None }, Source::User).unwrap();
        // Deux robots sur 127.0.0.1 : l'identifiant vient de l'adresse, la vérification de doublon aussi.
        assert_ne!(m.id, v.id);
        until(&hub, 10, "miio en ligne", |h| h.vacuum(&m.id).is_some_and(|x| x.online)).await;
        hub.vacuum_command(&m.id, Command::Locate, Source::User).await.unwrap();
        assert!(logged(&hub, Source::User, "Recherche du robot (signal sonore)"));
        assert!(hub.vacuum_command(&m.id, Command::Mode("mop".into()), Source::User).await.is_err(), "pas de mode sur un Roborock miIO");

        // Mauvais jeton : injoignable, puis « Hors ligne » (délais courts).
        hub.update_vacuum(&m.id, VacuumPatch { link: Some(Link::Miio { host: "127.0.0.1".into(), token: "00".repeat(16), port: Some(msim.port) }), ..Default::default() }, Source::User).unwrap();
        until(&hub, 40, "hors ligne", |h| h.problems_of(&m.id).iter().any(|p| p.kind == ProblemKind::Offline)).await;
        let p = hub.problems_of(&m.id).into_iter().find(|p| p.kind == ProblemKind::Offline).unwrap();
        assert!(p.detail.contains("robot éteint"), "{}", p.detail);
        assert!(!hub.alerts(300).iter().any(|a| a.message.contains(token) || a.message.contains(&"00".repeat(16))), "jeton dans le journal");

        hub.remove_vacuum(&m.id, Source::User).unwrap();
        hub.remove_vacuum(&v.id, Source::User).unwrap();
        assert!(hub.vacuums().is_empty());
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("homesmart.json")).unwrap()).unwrap();
        assert!(saved.get("vacuums").is_none(), "{saved}");
        hub.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
