//! Interfaccia in stile HP Click: impostazioni a sinistra, miniature al centro,
//! anteprima sul rotolo a destra, file trascinabili ovunque nella finestra.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use eframe::egui::load::SizedTexture;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, TextStyle, TextureHandle, TextureOptions, Vec2,
};

use crate::anteprima::{Anteprime, Chiave, PAGINE_DPI_MAX, PAGINE_MAX, Pagina};
use crate::config::Config;
use crate::file::{self, InfoFile};
use crate::foglio::{self, Foglio, SceltaFoglio};
use crate::impagina::{self, Allineamento, Impaginazione};
use crate::lavorazione::{self, Cornice, FormatoIso, Lavorazione, Scala};
use crate::nesting::{self, Nesting};
use crate::pjl::{self, Formato, OpzioniJob, Sorgente};
use crate::rotazione::{self, TOLLERANZA_MM};
use crate::stampante;
use crate::stato::{self, Cartuccia, StatoStampante};

const BLU_HP: Color32 = Color32::from_rgb(0x00, 0x7D, 0xBA);
const ARANCIO: Color32 = Color32::from_rgb(0xC0, 0x60, 0x00);
/// Stili di testo propri dell'interfaccia, oltre a quelli standard di egui.
const SEZIONE: &str = "Sezione";
const AZIONE: &str = "Azione";
const RICHIAMO: &str = "Richiamo";
const RILASCIO: &str = "Rilascio";
/// Famiglia del grassetto vero (in egui `.strong()` cambia solo il colore).
const GRASSETTO: &str = "Grassetto";

/// Tutte le dimensioni del testo, in un posto solo: in giro per il codice si
/// usano solo gli stili (`TextStyle`), mai valori fissi.
fn configura_stile(ctx: &egui::Context) {
    use FontFamily::{Monospace, Proportional};
    let mut font = egui::FontDefinitions::default();
    for (nome, dati) in [
        ("NotoSans-Regular", &include_bytes!("../assets/font/NotoSans-Regular.ttf")[..]),
        ("NotoSans-Bold", &include_bytes!("../assets/font/NotoSans-Bold.ttf")[..]),
    ] {
        font.font_data.insert(nome.into(), egui::FontData::from_static(dati).into());
    }
    // I font di default restano dopo, per i simboli (▸, ✓, …) che Noto Sans non ha.
    let ripiego = font.families[&Proportional].clone();
    font.families.get_mut(&Proportional).unwrap().insert(0, "NotoSans-Regular".into());
    let mut grassetto = vec!["NotoSans-Bold".to_string()];
    grassetto.extend(ripiego);
    font.families.insert(FontFamily::Name(GRASSETTO.into()), grassetto);
    ctx.set_fonts(font);
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Small, FontId::new(11.5, Proportional)),
            (TextStyle::Body, FontId::new(14.0, Proportional)),
            (TextStyle::Button, FontId::new(14.0, Proportional)),
            (TextStyle::Heading, FontId::new(20.0, Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, Monospace)),
            // Intestazioni di sezione (STAMPANTE, CARTA, …).
            (TextStyle::Name(SEZIONE.into()), FontId::new(12.0, FontFamily::Name(GRASSETTO.into()))),
            // Pulsante Stampa.
            (TextStyle::Name(AZIONE.into()), FontId::new(17.0, FontFamily::Name(GRASSETTO.into()))),
            // Invito ad aggiungere file nell'anteprima vuota.
            (TextStyle::Name(RICHIAMO.into()), FontId::new(20.0, Proportional)),
            // Sovrimpressione mentre si trascinano file.
            (TextStyle::Name(RILASCIO.into()), FontId::new(28.0, Proportional)),
        ]
        .into();
    });
}

fn stile(nome: &str) -> TextStyle {
    TextStyle::Name(nome.into())
}

/// Intestazione di sezione nel pannello delle impostazioni.
fn sezione(ui: &mut egui::Ui, testo: &str) {
    ui.label(RichText::new(testo).text_style(stile(SEZIONE)).strong());
}

const LARGHEZZE_ROTOLO: [(&str, f64); 3] = [("24\" (610 mm)", 610.0), ("36\" (914 mm)", 914.0), ("44\" (1118 mm)", 1118.0)];
/// Attesa del foglio singolo: dopo l'invio lo stato si rilegge ogni 5 s per
/// 2 minuti, per vedere quando la stampante chiede il foglio.
const ATTESA_FOGLIO: std::time::Duration = std::time::Duration::from_secs(120);
const PASSO_ATTESA_FOGLIO: std::time::Duration = std::time::Duration::from_secs(5);

/// Tipo di scala scelto nella GUI; i valori della modalità non attiva si
/// perdono quando si cambia, come in un pannello di stampa.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModoScala {
    Originale,
    Formato,
    Percentuale,
    Misura,
}

impl ModoScala {
    const TUTTI: [ModoScala; 4] =
        [ModoScala::Originale, ModoScala::Formato, ModoScala::Percentuale, ModoScala::Misura];

    fn di(scala: &Scala) -> Self {
        match scala {
            Scala::Originale => ModoScala::Originale,
            Scala::Formato(_) => ModoScala::Formato,
            Scala::Percentuale { .. } => ModoScala::Percentuale,
            Scala::Misura { .. } => ModoScala::Misura,
        }
    }

    fn etichetta(self) -> &'static str {
        match self {
            ModoScala::Originale => "Originale",
            ModoScala::Formato => "Formato",
            ModoScala::Percentuale => "Percentuale",
            ModoScala::Misura => "Misura",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum StatoJob {
    Pronto,
    InInvio(f32),
    Inviato,
    Errore(String),
}

struct Documento {
    /// Identificativo stabile: gli eventi dai thread non usano la posizione
    /// nella lista, che cambia quando si rimuove un file.
    id: u64,
    percorso: PathBuf,
    nome: String,
    info: InfoFile,
    /// Idoneità al nesting letta all'apertura, senza riaprire i TIFF a ogni frame.
    annidabile: bool,
    impronta: Option<(std::time::SystemTime, u64)>,
    copie: u32,
    stato: StatoJob,
    /// Scala scelta per questo documento (come le copie).
    lavorazione: Lavorazione,
    /// Lock delle proporzioni per percentuale e misura custom.
    blocca_proporzioni: bool,
    /// Impaginazione e lavorazione con cui sono state chieste le anteprime.
    anteprima: Chiave,
    /// Prime pagine renderizzate (vuoto finché non arrivano, o se l'anteprima
    /// non è disponibile).
    pagine: Vec<PaginaMostrata>,
    /// DPI effettivi più bassi delle immagini del documento, se misurati.
    dpi: Option<file::DpiStampa>,
    /// true se i DPI sono solo delle prime pagine del PDF.
    dpi_parziale: bool,
}

struct PaginaMostrata {
    tex: TextureHandle,
    /// Misure in mm, se note.
    mm: Option<(f64, f64)>,
    /// Impaginata da IQClock: parte dal margine sinistro del rotolo.
    impaginata: bool,
    /// Ruotata di 90° da IQClock per impaginarla sul rotolo.
    ruotata: bool,
}

impl Documento {
    fn adattabile_al_foglio(&self) -> bool {
        lavorazione::lavorabile(self.info.formato)
            && !(self.info.formato == Formato::Tiff && self.info.pagine > 1)
    }

    /// Rilegge misure e idoneità quando il file cambia. Il nuovo id scarta
    /// anche le anteprime ancora in arrivo per il contenuto precedente.
    fn aggiorna_file(&mut self, nuovo_id: u64) -> bool {
        let impronta = impronta_file(&self.percorso);
        if self.impronta == impronta {
            return false;
        }
        self.impronta = impronta;
        self.id = nuovo_id;
        self.pagine.clear();
        self.dpi = None;
        self.dpi_parziale = false;
        match file::analizza(&self.percorso) {
            Ok(info) => {
                self.annidabile = lavorazione::annidabile_file(&self.percorso, info.formato).unwrap_or(false);
                self.info = info;
            }
            Err(e) => {
                self.annidabile = false;
                self.info.mm = None;
                self.info.pagine_mm.clear();
                self.info.pagine = 0;
                self.stato = StatoJob::Errore(e.to_string());
            }
        }
        true
    }

    /// Misure della pagina `n` in mm: quelle date dall'anteprima, altrimenti
    /// quelle della prima pagina lette all'apertura del file, scalate.
    fn mm_pagina(&self, n: usize) -> Option<(f64, f64)> {
        // Una pagina impaginata da IQClock è rifatta dall'anteprima e le sue
        // misure sono quelle. Altrimenti valgono le misure lette dal file: il
        // render di pdfium le dà in punti e ignora `/UserUnit`, quindi una
        // pagina con UserUnit diverso da 1 arriverebbe a metà o al doppio.
        if let Some(p) = self.pagine.get(n)
            && p.impaginata
            && let Some(mm) = p.mm
        {
            return Some(mm);
        }
        // Con il nesting servono le misure di tutte le pagine: `pagine_mm` le
        // ha, `info.mm` vale solo per la prima.
        let base = self.info.pagine_mm.get(n).copied().flatten().or(self.info.mm);
        match base {
            Some((l, a)) => {
                let (sx, sy) = self.lavorazione.fattori(l, a);
                let cornice = self.cornice_mm();
                Some((l * sx + cornice, a * sy + cornice))
            }
            // File che lopdf non sa misurare ma pdfium sì.
            None => self.pagine.get(n).and_then(|p| p.mm),
        }
    }

    /// Spazio che la cornice aggiunge alle misure, in tutto (mm).
    fn cornice_mm(&self) -> f64 {
        self.lavorazione.cornice.map_or(0.0, |c| 2.0 * c.ingombro_mm())
    }

    /// Misure finali della prima pagina con la scala scelta: quelle mostrate
    /// sotto la sezione SCALA.
    fn uscita_mm(&self) -> Option<(f64, f64)> {
        let (l, a) = self.info.mm?;
        let (sx, sy) = self.lavorazione.fattori(l, a);
        let cornice = self.cornice_mm();
        Some((l * sx + cornice, a * sy + cornice))
    }

    /// true se la stampante ruoterà la pagina `n` di 90°. Senza misure non si
    /// può prevedere: la pagina resta dritta. Una pagina impaginata da IQClock
    /// è larga quanto la parte stampabile del rotolo e la stampante non la ruota più.
    fn ruotata(&self, n: usize, rotolo_mm: f64) -> bool {
        self.mm_pagina(n).is_some_and(|(l, a)| rotazione::ruota(l, a, rotolo_mm))
    }

    /// true se la pagina `n` esce ruotata di 90°, da IQClock o dalla stampante.
    fn esce_ruotata(&self, n: usize, rotolo_mm: f64) -> bool {
        self.pagine.get(n).is_some_and(|p| p.ruotata) || self.ruotata(n, rotolo_mm)
    }
}

/// Firma leggera del file: evita di riaprire e decodificare i TIFF invariati.
fn impronta_file(percorso: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(percorso).ok()?;
    Some((meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH), meta.len()))
}

enum Evento {
    Avanzamento(u64, f32),
    Finito(u64, Result<(), String>),
    Stato(Result<StatoStampante, String>),
    Pagine(u64, Chiave, Vec<Pagina>),
}

/// Finestra di attesa del foglio dopo un job su foglio singolo.
#[derive(Clone, Copy)]
struct AttesaFoglio {
    inizio: std::time::Instant,
    ultima: std::time::Instant,
    /// Foglio minimo che la stampante chiede per le pagine inviate, in lati
    /// (corto, lungo). «Foglio caricato» non basta: sulla T2600 un A4 su un
    /// foglio A4 resta in attesa di un foglio da 220 × 319 mm.
    minimo: Option<(f64, f64)>,
}

impl AttesaFoglio {
    fn nuova(minimo: Option<(f64, f64)>) -> Self {
        let ora = std::time::Instant::now();
        Self { inizio: ora, ultima: ora, minimo }
    }
}

/// Disposizione riutilizzata finché pezzi, misure e opzioni restano uguali.
struct CacheNesting {
    pezzi: Vec<nesting::Pezzo>,
    utile: f64,
    opzioni: Nesting,
    fogli: Vec<nesting::Foglio>,
}

pub struct App {
    config: Config,
    documenti: Vec<Documento>,
    prossimo_id: u64,
    anteprime: Anteprime,
    selezionato: Option<usize>,
    sorgente: Sorgente,
    /// Formato del foglio singolo (sezione CARTA): serve all'anteprima e
    /// all'avviso "la pagina non entra" e all'adattamento opzionale.
    foglio: Foglio,
    adatta_foglio: bool,
    scala_di_grigi: bool,
    taglierina: Option<bool>,
    /// Cornice di taglio attiva per tutti i documenti (sezione FINITURA).
    cornice: bool,
    /// Nesting attivo: più pezzi sullo stesso foglio (sezione NESTING).
    nesting: bool,
    nesting_ruota: bool,
    cache_nesting: Option<CacheNesting>,
    ultimo_controllo_file: std::time::Instant,
    extra_pjl: String,
    larghezza_rotolo: f64,
    /// Larghezza letta dalla stampante via SNMP (nome ingresso, mm).
    /// `None` = non esposta o stampante non raggiunta: si usa il fallback manuale.
    larghezza_rilevata: Option<(String, f64)>,
    allineamento: Allineamento,
    /// Larghezza confermata a mano per questa sessione, quando la stampante
    /// non la comunica (vedi `rotolo_verificato`).
    rotolo_confermato: Option<f64>,
    /// Finestra di conferma della larghezza del rotolo aperta.
    chiedi_conferma: bool,
    /// Impaginazione con cui sono state chieste le anteprime.
    impaginazione_anteprime: Option<Impaginazione>,
    stato_stampante: Option<Result<StatoStampante, String>>,
    verifica_in_corso: bool,
    /// Job su foglio singolo appena inviato: per due minuti lo stato si
    /// rilegge ogni 5 s (vedi `aggiorna_attesa_foglio`).
    attesa_foglio: Option<AttesaFoglio>,
    avvisi: Vec<String>,
    /// Con `IQCLOCK_FOTO=file.png` salva un'immagine della finestra e chiude
    /// (per documentazione e controlli visivi).
    foto: Option<(PathBuf, u32)>,
    tx: Sender<Evento>,
    rx: Receiver<Evento>,
}

impl App {
    /// Inizializza interfaccia e servizio anteprime, aggiunge i file iniziali
    /// e avvia la lettura asincrona dello stato della stampante.
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config, iniziali: Vec<PathBuf>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        configura_stile(&cc.egui_ctx);
        let (tx, rx) = channel();
        let (tx_pagine, ctx_pagine) = (tx.clone(), cc.egui_ctx.clone());
        let (anteprime, errore_pdfium) = Anteprime::avvia(move |id, chiave, pagine| {
            let _ = tx_pagine.send(Evento::Pagine(id, chiave, pagine));
            ctx_pagine.request_repaint();
        });
        let mut app = Self {
            larghezza_rotolo: config.larghezza_rotolo_mm,
            nesting: config.nesting,
            nesting_ruota: config.nesting_ruota,
            cache_nesting: None,
            ultimo_controllo_file: std::time::Instant::now(),
            allineamento: Allineamento::Sinistra,
            rotolo_confermato: None,
            chiedi_conferma: false,
            impaginazione_anteprime: None,
            config,
            documenti: Vec::new(),
            prossimo_id: 0,
            anteprime,
            selezionato: None,
            sorgente: Sorgente::Stampante,
            foglio: Foglio::default(),
            adatta_foglio: false,
            scala_di_grigi: false,
            taglierina: None,
            cornice: false,
            extra_pjl: String::new(),
            larghezza_rilevata: None,
            stato_stampante: None,
            verifica_in_corso: false,
            attesa_foglio: None,
            avvisi: errore_pdfium.into_iter().collect(),
            foto: std::env::var_os("IQCLOCK_FOTO").map(|p| (PathBuf::from(p), 0)),
            tx,
            rx,
        };
        for p in iniziali {
            app.aggiungi(p);
        }
        app.verifica_stampante(&cc.egui_ctx);
        app
    }

    /// Analizza un file, lo seleziona nella coda e ne richiede l’anteprima con
    /// la cornice corrente. Se non è leggibile, aggiunge un avviso.
    fn aggiungi(&mut self, percorso: PathBuf) {
        let nome = percorso.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match file::analizza(&percorso) {
            Ok(info) => {
                let id = self.prossimo_id;
                self.prossimo_id += 1;
                let lavorazione = Lavorazione {
                    cornice: self.cornice.then_some(self.cornice_attuale()),
                    ..Default::default()
                };
                let imp = self.impaginazione_anteprime;
                self.anteprime.richiedi(id, percorso.clone(), info.formato, lavorazione, imp);
                let annidabile = lavorazione::annidabile_file(&percorso, info.formato).unwrap_or(false);
                let impronta = impronta_file(&percorso);
                self.documenti.push(Documento {
                    id,
                    percorso,
                    nome,
                    info,
                    annidabile,
                    impronta,
                    copie: 1,
                    stato: StatoJob::Pronto,
                    lavorazione,
                    blocca_proporzioni: true,
                    anteprima: (imp, lavorazione),
                    pagine: Vec::new(),
                    dpi: None,
                    dpi_parziale: false,
                });
                self.selezionato = Some(self.documenti.len() - 1);
            }
            Err(e) => self.avvisi.push(format!("{nome}: {e}")),
        }
    }

    /// Dopo un job su foglio singolo rilegge lo stato ogni 5 s per 2 minuti,
    /// svegliando l'interfaccia col timer: così si vede quando la stampante
    /// prende il foglio. Sulla T2600 (06/11/2026) un job vero si è visto
    /// passare da «pronta» a «sta stampando» e tornare «pronta».
    fn aggiorna_attesa_foglio(&mut self, ctx: &egui::Context) {
        let Some(attesa) = self.attesa_foglio else {
            return;
        };
        if !self.su_foglio() || attesa.inizio.elapsed() >= ATTESA_FOGLIO {
            self.attesa_foglio = None;
            return;
        }
        if attesa.ultima.elapsed() >= PASSO_ATTESA_FOGLIO {
            self.attesa_foglio =
                Some(AttesaFoglio { ultima: std::time::Instant::now(), ..attesa });
            self.verifica_stampante(ctx);
        }
        ctx.request_repaint_after(PASSO_ATTESA_FOGLIO);
    }

    /// Riga dell'attesa del foglio: `None` se non c'è un job su foglio
    /// singolo appena inviato.
    fn attesa_foglio_testo(&self) -> Option<(String, Color32)> {
        let attesa = self.attesa_foglio?;
        if !self.su_foglio() {
            return None;
        }
        let letto = self.stato_stampante.as_ref()?.as_ref().ok()?;
        let ingresso = letto.manuale();
        let pronto = ingresso.is_some_and(|i| i.pronto());
        // Il foglio può essere caricato e comunque troppo piccolo: la T2600,
        // con un A4 per una pagina A4, lo tiene «in uso» e chiede un foglio da
        // 220 × 319 mm. Meglio dire quale foglio serve che «Foglio caricato».
        let stretto = attesa.minimo.filter(|_| pronto).and_then(|(min_corto, min_lungo)| {
            let (l, a) = letto.foglio_mm()?;
            let (corto, lungo) = (l.min(a), l.max(a));
            (corto + TOLLERANZA_MM < min_corto || lungo + TOLLERANZA_MM < min_lungo)
                .then_some((l, a, min_corto, min_lungo))
        });
        let (testo, colore) = if let Some((l, a, min_corto, min_lungo)) = stretto {
            (
                format!(
                    "Foglio troppo piccolo: è {l:.0} × {a:.0} mm, la stampante ne vuole almeno \
                     {min_corto:.0} × {min_lungo:.0} mm"
                ),
                ARANCIO,
            )
        } else if pronto {
            ("Foglio caricato".to_string(), Color32::DARK_GREEN)
        } else {
            let stato = ingresso
                .map(|i| i.descrizione_lunga())
                .unwrap_or_else(|| "ingresso manuale non dichiarato".into());
            (format!("In attesa del foglio — {stato}"), ARANCIO)
        };
        let resta = ATTESA_FOGLIO.saturating_sub(attesa.inizio.elapsed()).as_secs();
        Some((format!("{testo} (controllo per altri {resta} s)"), colore))
    }

    /// Foglio minimo che la stampante chiede per le pagine del documento, in
    /// lati (corto, lungo): il più grande fra le pagine. Senza misure note non
    /// si avvisa di niente.
    fn foglio_minimo_documento(&self, id: u64) -> Option<(f64, f64)> {
        let d = self.documenti.iter().find(|d| d.id == id)?;
        (0..d.info.pagine)
            .filter_map(|n| d.mm_pagina(n))
            .map(Foglio::minimo_lati)
            .reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)))
    }

    fn verifica_stampante(&mut self, ctx: &egui::Context) {
        if self.verifica_in_corso {
            return;
        }
        self.verifica_in_corso = true;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let host = self.config.host.clone();
        thread::spawn(move || {
            let r = stato::leggi(&host).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Evento::Stato(r));
            ctx.request_repaint();
        });
    }

    /// La cornice scelta in FINITURA: è un'opzione globale, ma ogni documento ne
    /// porta una copia nella sua `Lavorazione`.
    fn cornice_attuale(&self) -> Cornice {
        Cornice {
            distanza_mm: self.config.cornice_distanza_mm,
            spessore_mm: self.config.cornice_spessore_mm,
        }
    }

    /// true se la carta è il foglio singolo invece del rotolo.
    fn su_foglio(&self) -> bool {
        self.sorgente == Sorgente::Foglio
    }

    /// Il nesting vale solo sul rotolo: sul foglio singolo esce una pagina per
    /// foglio, composta dal file così com'è.
    fn nesting_attivo(&self) -> bool {
        self.nesting && !self.su_foglio()
    }

    fn area_foglio_attiva(&self) -> Option<(f64, f64)> {
        (self.su_foglio() && self.adatta_foglio).then(|| self.foglio.area_utile())
    }

    /// Misure che la stampante dichiara per il foglio caricato nell'ingresso
    /// manuale, se lo stato è stato letto.
    fn foglio_da_stampante(&self) -> Option<(f64, f64)> {
        self.stato_stampante.as_ref()?.as_ref().ok()?.foglio_mm()
    }

    fn impaginazione(&self) -> Option<Impaginazione> {
        // Con il nesting i fogli li compone IQClock: l'allineamento non c'entra.
        // Sul foglio singolo non c'è un rotolo su cui allineare.
        if self.su_foglio() || self.nesting_attivo() {
            return None;
        }
        (self.allineamento != Allineamento::Sinistra).then_some(Impaginazione {
            allineamento: self.allineamento,
            rotolo_mm: self.rotolo_effettivo(),
            margine_mm: self.config.margine_mm,
        })
    }

    /// true se la larghezza l'ha rilevata la stampante, o se l'utente ha
    /// confermato quella scelta a mano. Impaginando con una larghezza sbagliata
    /// la stampante ridurrebbe o taglierebbe la pagina.
    fn rotolo_verificato(&self) -> bool {
        self.larghezza_rilevata.is_some() || self.rotolo_confermato == Some(self.larghezza_rotolo)
    }

    /// Rilegge i file cambiati e rinnova la richiesta delle loro anteprime.
    fn aggiorna_file_documenti(&mut self) {
        for d in &mut self.documenti {
            if matches!(d.stato, StatoJob::Pronto | StatoJob::Errore(_)) && d.aggiorna_file(self.prossimo_id) {
                self.prossimo_id += 1;
                self.cache_nesting = None;
                self.anteprime.richiedi(d.id, d.percorso.clone(), d.info.formato, d.lavorazione, d.anteprima.0);
            }
        }
        self.ultimo_controllo_file = std::time::Instant::now();
    }

    /// Richiede di nuovo le anteprime quando cambiano impaginazione o scala:
    /// mostrano il file che parte davvero. Si aggiornano solo i documenti la
    /// cui lavorazione è cambiata.
    fn aggiorna_anteprime(&mut self) {
        let imp = self.impaginazione();
        let area_foglio = self.area_foglio_attiva();
        self.impaginazione_anteprime = imp;
        let anteprime = &self.anteprime;
        for d in &mut self.documenti {
            d.lavorazione.area_foglio_mm = area_foglio.filter(|_| d.adattabile_al_foglio());
            if !impagina::impaginabile(d.info.formato) {
                continue;
            }
            let chiave = (imp, d.lavorazione);
            if chiave != d.anteprima {
                d.anteprima = chiave;
                // Le pagine vecchie non sono più il file che parte: meglio la
                // pagina bianca del formato finché non arrivano le nuove. Anche i
                // DPI misurati sono superati: la scala è cambiata.
                d.pagine.clear();
                d.dpi = None;
                d.dpi_parziale = false;
                anteprime.richiedi(d.id, d.percorso.clone(), d.info.formato, d.lavorazione, imp);
            }
        }
    }

    fn opzioni(&self, doc: &Documento) -> OpzioniJob {
        OpzioniJob {
            nome: doc.nome.clone(),
            copie: doc.copie,
            sorgente: self.sorgente,
            scala_di_grigi: self.scala_di_grigi,
            taglierina: self.taglierina,
            extra: self.extra_pjl.clone(),
            impaginazione: self.impaginazione(),
            lavorazione: Lavorazione {
                area_foglio_mm: self.area_foglio_attiva(),
                ..doc.lavorazione
            },
        }
    }

    /// Prima di impaginare con una larghezza del rotolo non verificata chiede
    /// conferma.
    fn chiedi_e_stampa(&mut self, ctx: &egui::Context) {
        // Anche il nesting ha bisogno della larghezza giusta del rotolo.
        let da_impaginare = if self.nesting_attivo() {
            self.documenti.iter().any(|d| {
                matches!(d.stato, StatoJob::Pronto | StatoJob::Errore(_))
                    && nesting::annidabile(d.info.formato)
            })
        } else {
            self.impaginazione().is_some()
                && self.documenti.iter().any(|d| {
                    matches!(d.stato, StatoJob::Pronto | StatoJob::Errore(_))
                        && impagina::impaginabile(d.info.formato)
                })
        };
        if da_impaginare && !self.rotolo_verificato() {
            self.chiedi_conferma = true;
        } else {
            self.stampa(ctx);
        }
    }

    /// Larghezza usata per anteprima e controllo di ingombro: quella rilevata
    /// via SNMP se disponibile, altrimenti il fallback scelto a mano.
    fn rotolo_effettivo(&self) -> f64 {
        self.larghezza_rilevata.as_ref().map(|(_, m)| *m).unwrap_or(self.larghezza_rotolo)
    }

    /// Manda i documenti pronti uno dopo l'altro, su un unico thread: la porta
    /// 9100 accetta un job alla volta.
    fn stampa(&mut self, ctx: &egui::Context) {
        self.aggiorna_file_documenti();
        if self.nesting_attivo() {
            self.stampa_nesting(ctx);
            return;
        }
        let coda: Vec<_> = self
            .documenti
            .iter()
            .filter(|d| matches!(d.stato, StatoJob::Pronto | StatoJob::Errore(_)))
            .map(|d| (d.id, d.percorso.clone(), d.info.formato, self.opzioni(d)))
            .collect();
        for d in &mut self.documenti {
            if matches!(d.stato, StatoJob::Pronto | StatoJob::Errore(_)) {
                d.stato = StatoJob::InInvio(0.0);
            }
        }
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let (host, porta) = (self.config.host.clone(), self.config.porta);
        thread::spawn(move || {
            for (i, percorso, formato, opz) in coda {
                let mut ultimo = 0.0;
                let r = stampante::invia(&host, porta, &percorso, formato, &opz, |f| {
                    if f - ultimo >= 0.01 || f >= 1.0 {
                        ultimo = f;
                        let _ = tx.send(Evento::Avanzamento(i, f));
                        ctx.request_repaint();
                    }
                });
                let _ = tx.send(Evento::Finito(i, r.map_err(|e| format!("{e:#}"))));
                ctx.request_repaint();
            }
        });
    }

    /// Con il nesting attivo: compone i fogli (un job per foglio; tutti i
    /// documenti annidati seguono lo stesso stato), poi manda gli esclusi e i
    /// PostScript/HP-GL/2 uno alla volta come sempre.
    fn stampa_nesting(&mut self, ctx: &egui::Context) {
        let pronti: Vec<usize> = self
            .documenti
            .iter()
            .enumerate()
            .filter(|(_, d)| matches!(d.stato, StatoJob::Pronto | StatoJob::Errore(_)))
            .map(|(i, _)| i)
            .collect();
        let tutti: Vec<(u64, PathBuf, Formato, OpzioniJob)> = pronti
            .iter()
            .map(|&i| {
                let d = &self.documenti[i];
                (d.id, d.percorso.clone(), d.info.formato, self.opzioni(d))
            })
            .collect();
        let annidabili: Vec<lavorazione::DaAnnidare> = tutti
            .iter()
            .filter(|(_, _, formato, _)| nesting::annidabile(*formato))
            .map(|(id, percorso, formato, opz)| lavorazione::DaAnnidare {
                id: *id,
                percorso: percorso.clone(),
                formato: *formato,
                lavorazione: opz.lavorazione,
                copie: opz.copie,
            })
            .collect();
        for d in &mut self.documenti {
            if matches!(d.stato, StatoJob::Pronto | StatoJob::Errore(_)) {
                d.stato = StatoJob::InInvio(0.0);
            }
        }
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let (host, porta) = (self.config.host.clone(), self.config.porta);
        let (rotolo, margine) = (self.rotolo_effettivo(), self.config.margine_mm);
        let nesting = Nesting {
            distanza_mm: self.config.nesting_distanza_mm,
            ruota: self.nesting_ruota,
        };
        let base = OpzioniJob {
            nome: String::new(),
            copie: 1,
            sorgente: self.sorgente,
            scala_di_grigi: self.scala_di_grigi,
            taglierina: self.taglierina,
            extra: self.extra_pjl.clone(),
            impaginazione: None,
            lavorazione: Lavorazione::default(),
        };
        thread::spawn(move || {
            let invia = |percorso: &std::path::Path,
                         formato: Formato,
                         opz: &OpzioniJob,
                         ids: &[u64]|
             -> Result<(), String> {
                let mut ultimo = 0.0;
                stampante::invia(&host, porta, percorso, formato, opz, |f| {
                    if f - ultimo >= 0.01 || f >= 1.0 {
                        ultimo = f;
                        for id in ids {
                            let _ = tx.send(Evento::Avanzamento(*id, f));
                        }
                        ctx.request_repaint();
                    }
                })
                .map_err(|e| format!("{e:#}"))
            };
            let esclusi = match lavorazione::componi_nesting(&annidabili, rotolo, margine, &nesting) {
                Err(e) => {
                    let errore = format!("{e:#}");
                    for d in &annidabili {
                        let _ = tx.send(Evento::Finito(d.id, Err(errore.clone())));
                    }
                    ctx.request_repaint();
                    Vec::new()
                }
                Ok(annidamento) => {
                    // Un documento può finire su più fogli: vale l'esito peggiore,
                    // se no un foglio fallito seguito da uno riuscito farebbe
                    // credere che sia uscito tutto.
                    let mut esiti: std::collections::HashMap<u64, Result<(), String>> =
                        std::collections::HashMap::new();
                    for foglio in &annidamento.fogli {
                        let opz = OpzioniJob {
                            nome: format!("Nesting ({} file)", annidamento.documenti),
                            ..base.clone()
                        };
                        let r = invia(foglio.file.percorso(), Formato::Pdf, &opz, &foglio.documenti);
                        for id in &foglio.documenti {
                            let esito = esiti.entry(*id).or_insert_with(|| Ok(()));
                            if esito.is_ok() {
                                *esito = r.clone();
                            }
                        }
                    }
                    for (id, esito) in esiti {
                        let _ = tx.send(Evento::Finito(id, esito));
                    }
                    ctx.request_repaint();
                    annidamento.esclusi
                }
            };
            // Documenti esclusi dal nesting e formati che non si annidano.
            for (id, percorso, formato, opz) in &tutti {
                if !esclusi.contains(id) && nesting::annidabile(*formato) {
                    continue;
                }
                let r = invia(percorso, *formato, opz, &[*id]);
                let _ = tx.send(Evento::Finito(*id, r));
                ctx.request_repaint();
            }
        });
    }

    /// Salva il job come file .prn, per controllarlo o mandarlo a mano.
    fn salva_prn(&mut self, i: usize) {
        let doc = &self.documenti[i];
        let Some(dest) = rfd::FileDialog::new()
            .set_file_name(format!("{}.prn", doc.nome))
            .add_filter("File di stampa", &["prn"])
            .save_file()
        else {
            return;
        };
        let r = std::fs::File::create(&dest).map_err(anyhow::Error::from).and_then(|mut f| {
            stampante::scrivi_job(&mut f, &doc.percorso, doc.info.formato, &self.opzioni(doc), |_| {})
        });
        if let Err(e) = r {
            self.avvisi.push(format!("Salvataggio non riuscito: {e:#}"));
        }
    }

    fn gestisci_eventi(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Evento::Avanzamento(id, f) => self.imposta_stato(id, StatoJob::InInvio(f)),
                Evento::Finito(id, Ok(())) => {
                    self.imposta_stato(id, StatoJob::Inviato);
                    // Con il foglio singolo la stampante lo chiede dopo aver
                    // ricevuto il job: da qui si guarda l'ingresso manuale.
                    if self.su_foglio() {
                        self.attesa_foglio =
                            Some(AttesaFoglio::nuova(self.foglio_minimo_documento(id)));
                    }
                }
                Evento::Finito(id, Err(e)) => self.imposta_stato(id, StatoJob::Errore(e)),
                Evento::Stato(r) => {
                    match &r {
                        Ok(s) => {
                            self.larghezza_rilevata =
                                s.larghezza_rotolo_mm().map(|(n, m)| (n.to_string(), m));
                            // Con il foglio singolo contano le misure che la
                            // stampante dichiara per l'ingresso manuale.
                            self.foglio.aggiorna_da_stampante(s.foglio_mm());
                        }
                        Err(_) => self.larghezza_rilevata = None,
                    }
                    self.stato_stampante = Some(r);
                    self.verifica_in_corso = false;
                }
                Evento::Pagine(id, chiave, pagine) => {
                    // Se nel frattempo è cambiata scala o impaginazione, queste
                    // pagine sono superate: arriveranno quelle giuste.
                    if let Some(d) = self.documenti.iter_mut().find(|d| d.id == id && d.anteprima == chiave) {
                        // I DPI sono del documento (il minimo fra le pagine):
                        // arrivano uguali su tutte.
                        d.dpi = pagine.first().and_then(|p| {
                            Some(file::DpiStampa { dpi: p.dpi_min?, presunti: p.dpi_presunti })
                        });
                        d.dpi_parziale = pagine.first().is_some_and(|p| p.dpi_parziale);
                        d.pagine = pagine
                            .into_iter()
                            .enumerate()
                            .map(|(n, p)| {
                                let tex = ctx.load_texture(format!("{}-{n}", d.id), p.immagine, TextureOptions::LINEAR);
                                PaginaMostrata { tex, mm: p.mm, impaginata: p.impaginata, ruotata: p.ruotata }
                            })
                            .collect();
                    }
                }
            }
        }
    }

    fn imposta_stato(&mut self, id: u64, stato: StatoJob) {
        if let Some(d) = self.documenti.iter_mut().find(|d| d.id == id) {
            d.stato = stato;
        }
    }

    fn file_trascinati(&mut self, ctx: &egui::Context) {
        let caduti: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        for p in caduti {
            self.aggiungi(p);
        }
    }

    /// Mostra i controlli di stampa in un pannello scorrevole e propaga
    /// la cornice scelta alle lavorazioni dei documenti già in coda.
    fn pannello_impostazioni(&mut self, ui: &mut egui::Ui) {
        // Le sezioni sono più alte della finestra minima: scorrono, mentre il
        // pulsante Stampa resta fermo in fondo.
        let altezza = (ui.available_height() - 104.0).max(160.0);
        egui::ScrollArea::vertical().max_height(altezza).show(ui, |ui| {
        ui.add_space(8.0);
        ui.heading("Impostazioni di stampa");
        ui.add_space(8.0);

        sezione(ui, "STAMPANTE");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.config.host).desired_width(130.0));
            ui.label(":");
            ui.add(egui::DragValue::new(&mut self.config.porta));
        });
        ui.horizontal(|ui| {
            if ui.button("Aggiorna stato").clicked() {
                self.verifica_stampante(ui.ctx());
            }
            if self.verifica_in_corso {
                ui.spinner();
            }
        });
        match &self.stato_stampante {
            Some(Ok(s)) => {
                if let Some(m) = &s.modello {
                    ui.label(RichText::new(m).strong());
                }
                let colore = if s.ok() { Color32::DARK_GREEN } else { ARANCIO };
                ui.colored_label(colore, s.descrizione());
                for i in &s.ingressi {
                    let colore = if i.pronto() { Color32::DARK_GREEN } else { Color32::GRAY };
                    let nome = match i.nome.as_str() {
                        "ROLL" => "Rotolo",
                        "MANUAL_FEED" => "Foglio singolo",
                        n => n,
                    };
                    ui.colored_label(colore, format!("{nome}: {}", i.descrizione_lunga()));
                }
                if let Some((testo, colore)) = self.attesa_foglio_testo() {
                    ui.colored_label(colore, testo);
                }
                for c in &s.cartucce {
                    barra_inchiostro(ui, c);
                }
            }
            Some(Err(e)) => {
                ui.colored_label(Color32::RED, e);
            }
            None => {}
        }

        ui.separator();
        sezione(ui, "CARTA");
        egui::ComboBox::from_id_salt("sorgente")
            .selected_text(self.sorgente.etichetta())
            .width(200.0)
            .show_ui(ui, |ui| {
                for s in Sorgente::TUTTE {
                    ui.selectable_value(&mut self.sorgente, s, s.etichetta());
                }
            });
        if self.su_foglio() {
            // Sul foglio la carta è già tagliata: del rotolo non serve niente,
            // serve il formato per disegnare l'anteprima e per l'avviso se la
            // pagina non ci entra, o per ridurla se l'utente lo chiede.
            egui::ComboBox::from_id_salt("foglio-formato")
                .selected_text(self.foglio.etichetta())
                .width(200.0)
                .show_ui(ui, |ui| {
                    let letto = self.foglio_da_stampante();
                    if ui
                        .selectable_label(
                            self.foglio.scelta == SceltaFoglio::Stampante,
                            "Dalla stampante",
                        )
                        .clicked()
                    {
                        self.foglio.scelta = SceltaFoglio::Stampante;
                        self.foglio.aggiorna_da_stampante(letto);
                    }
                    ui.separator();
                    for f in foglio::FORMATI {
                        if ui
                            .selectable_label(
                                self.foglio.scelta == SceltaFoglio::Iso(f),
                                f.etichetta(),
                            )
                            .clicked()
                        {
                            self.foglio = Foglio::iso(f, !self.foglio.verticale());
                        }
                    }
                    ui.separator();
                    if ui
                        .selectable_label(self.foglio.scelta == SceltaFoglio::Mano, "Personalizzato")
                        .clicked()
                    {
                        self.foglio.scelta = SceltaFoglio::Mano;
                    }
                })
                .response
                .on_hover_text(
                    "Formato del foglio caricato: l'anteprima lo disegna e avvisa se la pagina non entra",
                );
            match self.foglio.scelta {
                SceltaFoglio::Iso(_) => {
                    ui.horizontal(|ui| {
                        let verticale = self.foglio.verticale();
                        if ui.selectable_label(verticale, "Verticale").clicked() && !verticale {
                            self.foglio.ruota();
                        }
                        if ui.selectable_label(!verticale, "Orizzontale").clicked() && verticale {
                            self.foglio.ruota();
                        }
                    });
                }
                SceltaFoglio::Mano => {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.foglio.larghezza_mm)
                                .range(100.0..=1500.0)
                                .speed(1.0)
                                .suffix(" mm"),
                        );
                        ui.label("×");
                        ui.add(
                            egui::DragValue::new(&mut self.foglio.altezza_mm)
                                .range(100.0..=1500.0)
                                .speed(1.0)
                                .suffix(" mm"),
                        );
                    });
                }
                SceltaFoglio::Stampante => {
                    let nota = match self.foglio_da_stampante() {
                        Some(_) => "Misure lette dalla stampante: aggiorna con Aggiorna stato",
                        None => "La stampante non dichiara le misure del foglio: l'anteprima usa l'ultimo formato noto",
                    };
                    ui.colored_label(Color32::GRAY, nota);
                }
            }
            ui.checkbox(&mut self.adatta_foglio, "Adatta all'area stampabile")
                .on_hover_text("Riduce solo le pagine troppo grandi, mantenendo le proporzioni. Riserva 5 mm per lato e 22 mm sul lato d'entrata; la scala del disegno cambia");
            if self.adatta_foglio {
                let (corto, lungo) = self.foglio.area_utile();
                ui.label(RichText::new(format!("Area utile: {corto:.0} × {lungo:.0} mm")).small());
                ui.label(RichText::new("PostScript, HP-GL/2 e TIFF multipagina non supportano l'adattamento").small());
            }
        } else {
            egui::ComboBox::from_id_salt("rotolo")
                .selected_text(
                    LARGHEZZE_ROTOLO
                        .iter()
                        .find(|(_, mm)| *mm == self.larghezza_rotolo)
                        .map(|(n, _)| *n)
                        .unwrap_or("?"),
                )
                .width(200.0)
                .show_ui(ui, |ui| {
                    for (n, mm) in LARGHEZZE_ROTOLO {
                        ui.selectable_value(&mut self.larghezza_rotolo, mm, n);
                    }
                })
                .response
                .on_hover_text(
                    "Fallback manuale: vale solo se la stampante non dice che rotolo ha caricato",
                );
            match &self.larghezza_rilevata {
                Some((nome, mm)) => {
                    ui.colored_label(
                        Color32::DARK_GREEN,
                        format!("Rotolo rilevato ({nome}): {mm:.0} mm — anteprima su questo"),
                    );
                }
                None => {
                    let nota = match &self.stato_stampante {
                        Some(Ok(_)) => "Rotolo non rilevato (il suo ingresso non dichiara la larghezza): uso il fallback manuale",
                        Some(Err(_)) | None => "Rotolo non rilevato: uso il fallback manuale",
                    };
                    ui.colored_label(Color32::GRAY, nota);
                }
            }
        }

        ui.add_space(6.0);
        sezione(ui, "ALLINEAMENTO");
        let escluso = self
            .selezionato
            .and_then(|i| self.documenti.get(i))
            .is_some_and(|d| !impagina::impaginabile(d.info.formato));
        ui.add_enabled_ui(!escluso && !self.nesting_attivo() && !self.su_foglio(), |ui| {
            ui.horizontal(|ui| {
                for a in Allineamento::TUTTI {
                    ui.selectable_value(&mut self.allineamento, a, a.etichetta());
                }
            })
            .response
            .on_hover_text("Posizione sul rotolo del contenuto più stretto della carta. Con Centro e Destra IQClock manda una pagina larga quanto il rotolo meno i margini e la ruota da sé quando conviene");
        });
        if escluso {
            ui.label(RichText::new("PostScript e HP-GL/2 partono così come sono: l'allineamento non si applica").small());
        }
        if self.nesting_attivo() {
            ui.label(RichText::new("Nesting attivo: l'allineamento non si applica").small());
        }
        if self.su_foglio() {
            ui.label(RichText::new("Con il foglio singolo l'allineamento non si applica").small());
        }
        if !self.su_foglio() && (self.allineamento != Allineamento::Sinistra || self.nesting) {
            ui.horizontal(|ui| {
                ui.label("Margine");
                ui.add(egui::DragValue::new(&mut self.config.margine_mm).range(impagina::MARGINE_MIN_MM..=50.0).speed(0.5).suffix(" mm"))
            })
            .response
            .on_hover_text("Bordo non stampabile per lato: la pagina mandata è larga quanto il rotolo meno questo margine sui due lati");
        }

        ui.add_space(6.0);
        sezione(ui, "NESTING");
        ui.add_enabled_ui(!self.su_foglio(), |ui| {
            ui.checkbox(&mut self.nesting, "Nesting")
                .on_hover_text("Mette più file sullo stesso foglio, un job per foglio; PostScript e HP-GL/2 partono da soli");
        });
        if self.su_foglio() {
            ui.label(RichText::new("Con il foglio singolo il nesting non si applica").small());
        }
        if self.nesting_attivo() {
            ui.checkbox(&mut self.nesting_ruota, "Ruota i pezzi")
                .on_hover_text("Sdraiato il lato lungo va lungo il rotolo: consuma meno carta");
            ui.horizontal(|ui| {
                ui.label("Distanza");
                ui.add(
                    egui::DragValue::new(&mut self.config.nesting_distanza_mm)
                        .range(0.0..=50.0)
                        .speed(0.5)
                        .suffix(" mm"),
                );
            })
            .response
            .on_hover_text("Spazio tra i pezzi e tra le file");

            // Le copie sono del pezzo: si scelgono qui, con l'anteprima dei
            // fogli sott'occhio, e sono le stesse della sezione COPIE del file.
            ui.add_space(4.0);
            ui.label(RichText::new("Copie per pezzo").small().strong())
                .on_hover_text("Quante volte ogni file entra nei fogli: l'anteprima si aggiorna");
            let annidabili: Vec<usize> = self
                .documenti
                .iter()
                .enumerate()
                .filter(|(_, d)| nesting::annidabile(d.info.formato))
                .map(|(i, _)| i)
                .collect();
            if annidabili.is_empty() {
                ui.label(RichText::new("Nessun PDF o immagine in coda").small());
            }
            let mut da_selezionare = None;
            for i in annidabili {
                let nome_breve = (ui.available_width() - 58.0).max(60.0);
                ui.horizontal(|ui| {
                    let d = &mut self.documenti[i];
                    let altezza = ui.text_style_height(&TextStyle::Small);
                    ui.allocate_ui_with_layout(
                        Vec2::new(nome_breve, altezza),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            let etichetta = ui
                                .add(egui::Label::new(RichText::new(&d.nome).small()).truncate());
                            if etichetta.clicked() {
                                da_selezionare = Some(i);
                            }
                            etichetta.on_hover_text(format!("{}: {} copie", d.nome, d.copie));
                        },
                    );
                    ui.add(egui::DragValue::new(&mut d.copie).range(1..=999));
                });
            }
            if let Some(i) = da_selezionare {
                self.selezionato = Some(i);
            }
        }

        ui.add_space(6.0);
        sezione(ui, "COLORE");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.scala_di_grigi, false, "Colore");
            ui.selectable_value(&mut self.scala_di_grigi, true, "Scala di grigi");
        });

        ui.add_space(6.0);
        sezione(ui, "TAGLIERINA");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.taglierina, None, "Stampante");
            ui.selectable_value(&mut self.taglierina, Some(true), "Sì");
            ui.selectable_value(&mut self.taglierina, Some(false), "No");
        })
        .response
        .on_hover_text("CUTTER non è nel manuale PJL: verificare che la T2600 lo rispetti");

        ui.add_space(6.0);
        sezione(ui, "FINITURA");
        ui.checkbox(&mut self.cornice, "Cornice di taglio")
            .on_hover_text("Linea continua attorno al contenuto: il foglio cresce di distanza più spessore per lato");
        if self.cornice {
            // Una riga per misura: affiancate non starebbero nella colonna.
            ui.horizontal(|ui| {
                ui.label("Distanza");
                ui.add(
                    egui::DragValue::new(&mut self.config.cornice_distanza_mm)
                        .range(0.0..=50.0)
                        .speed(0.5)
                        .suffix(" mm"),
                );
            })
            .response
            .on_hover_text("Dalla linea al bordo del contenuto");
            ui.horizontal(|ui| {
                ui.label("Spessore");
                ui.add(
                    egui::DragValue::new(&mut self.config.cornice_spessore_mm)
                        .range(0.1..=5.0)
                        .speed(0.05)
                        .suffix(" mm"),
                );
            })
            .response
            .on_hover_text("Spessore della linea stampata");
            if let Some(i) = self.selezionato
                && !lavorazione::lavorabile(self.documenti[i].info.formato)
            {
                ui.label(
                    RichText::new("PostScript e HP-GL/2 partono così come sono: la cornice non si applica")
                        .small(),
                );
            }
        }
        // La cornice è un'opzione di finitura globale ma viaggia nella
        // lavorazione di ogni documento (anteprima, impaginazione e nesting la
        // trattano per pezzo): qui si allineano quelli già in lista.
        let cornice = self.cornice.then_some(self.cornice_attuale());
        let area_foglio = self.area_foglio_attiva();
        for d in &mut self.documenti {
            d.lavorazione.cornice = cornice;
            d.lavorazione.area_foglio_mm = area_foglio.filter(|_| d.adattabile_al_foglio());
        }

        if let Some(i) = self.selezionato {
            ui.add_space(6.0);
            sezione(ui, "COPIE");
            {
                let d = &mut self.documenti[i];
                ui.horizontal(|ui| {
                    if ui.button("−").clicked() && d.copie > 1 {
                        d.copie -= 1;
                    }
                    ui.add(egui::DragValue::new(&mut d.copie).range(1..=999));
                    if ui.button("+").clicked() {
                        d.copie += 1;
                    }
                });
            }

            ui.add_space(6.0);
            sezione(ui, "SCALA");
            if !lavorazione::lavorabile(self.documenti[i].info.formato) {
                ui.label(
                    RichText::new(
                        "PostScript e HP-GL/2 partono così come sono: la scala non si applica",
                    )
                    .small(),
                );
            } else {
                // Misure originali: il lock delle proporzioni non deve ereditare una scala che deforma.
                let prima = self.documenti[i].info.mm;
                let d = &mut self.documenti[i];
                let mut modo = ModoScala::di(&d.lavorazione.scala);
                egui::ComboBox::from_id_salt("scala-modo")
                    .selected_text(modo.etichetta())
                    .width(200.0)
                    .show_ui(ui, |ui| {
                        for m in ModoScala::TUTTI {
                            ui.selectable_value(&mut modo, m, m.etichetta());
                        }
                    });
                if modo != ModoScala::di(&d.lavorazione.scala) {
                    d.lavorazione.scala = match modo {
                        ModoScala::Originale => Scala::Originale,
                        ModoScala::Formato => Scala::Formato(FormatoIso::A3),
                        ModoScala::Percentuale => Scala::Percentuale { x: 100.0, y: 100.0 },
                        ModoScala::Misura => {
                            let (l, a) = prima.unwrap_or((210.0, 297.0));
                            Scala::Misura {
                                larghezza_mm: l,
                                altezza_mm: a,
                                proporzioni: d.blocca_proporzioni,
                            }
                        }
                    };
                }
                let blocco = d.blocca_proporzioni;
                let (rapporto_l, rapporto_a) = prima.unwrap_or((210.0, 297.0));
                let rapporto = if rapporto_l > 0.0 && rapporto_a > 0.0 {
                    rapporto_l / rapporto_a
                } else {
                    210.0 / 297.0
                };
                let ((min_l, min_a), (max_l, max_a)) =
                    lavorazione::limiti_mm(rapporto_l.max(1.0), rapporto_a.max(1.0));
                match &mut d.lavorazione.scala {
                    Scala::Originale => {}
                    Scala::Formato(formato) => {
                        egui::ComboBox::from_id_salt("scala-formato")
                            .selected_text(formato.etichetta())
                            .width(100.0)
                            .show_ui(ui, |ui| {
                                for f in FormatoIso::TUTTI {
                                    ui.selectable_value(formato, f, f.etichetta());
                                }
                            });
                    }
                    Scala::Percentuale { x, y } => {
                        ui.horizontal(|ui| {
                            ui.label("Percentuale");
                            let mut nuovo_x = *x;
                            let cambiata = ui
                                .add(
                                    egui::DragValue::new(&mut nuovo_x)
                                        .range(1.0..=1000.0)
                                        .speed(1.0)
                                        .suffix(" %"),
                                )
                                .changed();
                            if cambiata {
                                *x = nuovo_x;
                                if blocco {
                                    *y = nuovo_x;
                                }
                            }
                            if !blocco {
                                ui.label("×");
                                ui.add(
                                    egui::DragValue::new(y)
                                        .range(1.0..=1000.0)
                                        .speed(1.0)
                                        .suffix(" %"),
                                );
                            }
                        });
                    }
                    Scala::Misura { larghezza_mm, altezza_mm, proporzioni } => {
                        *proporzioni = blocco;
                        let (vecchia_l, vecchia_a) = (*larghezza_mm, *altezza_mm);
                        ui.horizontal(|ui| {
                            ui.label("Misura");
                            ui.add(
                                egui::DragValue::new(larghezza_mm)
                                    .range(min_l..=max_l)
                                    .speed(1.0)
                                    .suffix(" mm"),
                            );
                            ui.label("×");
                            ui.add(
                                egui::DragValue::new(altezza_mm)
                                    .range(min_a..=max_a)
                                    .speed(1.0)
                                    .suffix(" mm"),
                            );
                        });
                        if blocco {
                            if *larghezza_mm != vecchia_l {
                                *altezza_mm = *larghezza_mm / rapporto;
                            } else if *altezza_mm != vecchia_a {
                                *larghezza_mm = *altezza_mm * rapporto;
                            }
                        }
                    }
                }
                let proporzioni_visibili =
                    modo != ModoScala::Formato && modo != ModoScala::Originale;
                if proporzioni_visibili
                    && ui.checkbox(&mut d.blocca_proporzioni, "Blocca proporzioni").changed()
                {
                    let blocco = d.blocca_proporzioni;
                    match &mut d.lavorazione.scala {
                        Scala::Percentuale { x, y } if blocco => *y = *x,
                        Scala::Misura { proporzioni, .. } => *proporzioni = blocco,
                        _ => {}
                    }
                }
                match d.uscita_mm() {
                    Some((l, a)) => {
                        ui.label(format!("Esce: {l:.0} × {a:.0} mm{}", file::formato_iso(l, a)));
                    }
                    None => {
                        ui.label("Esce: misure non note");
                    }
                }
            }
        }

        ui.add_space(6.0);
        egui::CollapsingHeader::new("Avanzate").show(ui, |ui| {
            // La soglia sta qui finché non c'è la finestra impostazioni (#18).
            ui.horizontal(|ui| {
                ui.label("Avvisa sotto");
                ui.add(
                    egui::DragValue::new(&mut self.config.soglia_dpi)
                        .range(50.0..=600.0)
                        .speed(10.0)
                        .suffix(" dpi"),
                );
            })
            .response
            .on_hover_text("Sotto questi DPI effettivi la miniatura e l'anteprima avvisano; la stampa non viene bloccata");
            ui.separator();
            ui.label("Righe @PJL aggiuntive:");
            ui.add(
                egui::TextEdit::multiline(&mut self.extra_pjl)
                    .hint_text("@PJL SET MIRROR=OFF")
                    .font(egui::TextStyle::Monospace)
                    .desired_rows(3),
            );
            if let Some(i) = self.selezionato {
                let d = &self.documenti[i];
                let testo = String::from_utf8_lossy(&pjl::intestazione(&self.opzioni(d), d.info.formato))
                    .replace('\x1b', "<ESC>")
                    .replace('\r', "");
                ui.label("Intestazione che verrà mandata:");
                ui.label(RichText::new(testo).monospace().small());
                if ui.button("Salva come .prn…").clicked() {
                    self.salva_prn(i);
                }
            }
        });
        });

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(10.0);
            let pronti = self.documenti.iter().filter(|d| !matches!(d.stato, StatoJob::InInvio(_) | StatoJob::Inviato)).count();
            let pulsante = egui::Button::new(RichText::new("Stampa").color(Color32::WHITE).text_style(stile(AZIONE)))
                .fill(BLU_HP)
                .corner_radius(CornerRadius::same(16))
                .min_size(Vec2::new(ui.available_width(), 36.0));
            if ui.add_enabled(pronti > 0, pulsante).clicked() {
                self.chiedi_e_stampa(ui.ctx());
            }
            if pronti > 1 {
                ui.label(format!("{pronti} file da stampare"));
            }
            // Pagine dei PDF in coda per formato, copie comprese.
            let mut formati = Vec::new();
            for d in self.documenti.iter().filter(|d| !matches!(d.stato, StatoJob::InInvio(_) | StatoJob::Inviato)) {
                for (f, n) in &d.info.formati_pagine {
                    file::aggiungi_formato(&mut formati, f.clone(), n * d.copie as usize);
                }
            }
            if !formati.is_empty() {
                ui.label(format!("Totale: {}", file::etichetta_formati(&formati)));
            }
        });
    }

    /// Mostra i documenti in coda e permette di selezionarli o rimuoverli.
    fn pannello_miniature(&mut self, ui: &mut egui::Ui) {
        let mut da_togliere = None;
        let soglia = self.config.soglia_dpi;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, d) in self.documenti.iter().enumerate() {
                let selezionato = self.selezionato == Some(i);
                let cornice = egui::Frame::new()
                    .inner_margin(6.0)
                    .corner_radius(4.0)
                    .stroke(if selezionato { Stroke::new(2.0, BLU_HP) } else { Stroke::new(1.0, Color32::from_gray(210)) })
                    .fill(Color32::WHITE);
                let r = cornice
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 90.0), Sense::hover());
                        disegna_pagina(ui, rect, d, 0, false);
                        ui.label(RichText::new(&d.nome).small().strong());
                        let mut didascalia =
                            format!("{} · {}", d.info.formato.etichetta(), file::dimensione_leggibile(d.info.byte));
                        if !d.lavorazione.scala.originale() {
                            didascalia.push_str(&format!(" · {}", d.lavorazione.scala.didascalia()));
                        }
                        if (0..d.info.pagine.min(PAGINE_MAX)).any(|n| !self.su_foglio() && d.esce_ruotata(n, self.rotolo_effettivo())) {
                            didascalia.push_str(" · 90°");
                        }
                        ui.label(RichText::new(didascalia).small());
                        if !d.info.formati_pagine.is_empty() {
                            ui.label(RichText::new(file::etichetta_formati(&d.info.formati_pagine)).small());
                        }
                        match d.dpi {
                            Some(dpi) => {
                                let testo = RichText::new(dpi.etichetta(soglia)).small();
                                ui.label(if dpi.sotto_soglia(soglia) {
                                    testo.color(ARANCIO).strong()
                                } else {
                                    testo
                                });
                            }
                            // Senza immagini raster non c'è un DPI da mostrare. Con
                            // la misura ferma alle prime pagine non si può dire che
                            // il resto del documento sia vettoriale.
                            None if d.info.formato == Formato::Pdf && !d.pagine.is_empty() => {
                                let testo = if d.dpi_parziale {
                                    format!("vettoriale finora (prime {PAGINE_DPI_MAX} pagine)")
                                } else {
                                    "vettoriale".to_string()
                                };
                                ui.label(RichText::new(testo).small());
                            }
                            // Anteprima non ancora arrivata: i DPI del file.
                            None => {
                                if let Some(r) = &d.info.risoluzione {
                                    let testo = RichText::new(r.etichetta()).small();
                                    ui.label(if r.presunta { testo.color(ARANCIO) } else { testo });
                                }
                            }
                        }
                        match &d.stato {
                            StatoJob::Pronto => {}
                            StatoJob::InInvio(f) => {
                                ui.add(egui::ProgressBar::new(*f).desired_height(6.0));
                            }
                            StatoJob::Inviato => {
                                ui.colored_label(Color32::DARK_GREEN, "inviato");
                            }
                            StatoJob::Errore(e) => {
                                ui.colored_label(Color32::RED, "errore (dettagli col mouse)").on_hover_text(e);
                            }
                        }
                    })
                    .response
                    .interact(Sense::click());
                if r.clicked() {
                    self.selezionato = Some(i);
                }
                r.context_menu(|ui| {
                    if ui.button("Rimuovi").clicked() {
                        da_togliere = Some(i);
                    }
                });
                ui.add_space(6.0);
            }
            if ui.button("+ Aggiungi file…").clicked()
                && let Some(files) = rfd::FileDialog::new()
                    .add_filter("Stampabili", &["pdf", "ps", "eps", "tif", "tiff", "jpg", "jpeg", "png", "plt", "hpgl"])
                    .pick_files()
            {
                for f in files {
                    self.aggiungi(f);
                }
            }
        });
        if let Some(i) = da_togliere
            && !matches!(self.documenti[i].stato, StatoJob::InInvio(_))
        {
            self.documenti.remove(i);
            self.selezionato = (!self.documenti.is_empty()).then(|| i.min(self.documenti.len() - 1));
        }
    }

    /// Disegna il documento selezionato sul rotolo, oppure il nesting della coda.
    fn anteprima(&mut self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, Color32::from_gray(225));
        if self.nesting_attivo() {
            self.anteprima_nesting(ui, area);
            return;
        }
        let Some(d) = self.selezionato.and_then(|i| self.documenti.get(i)) else {
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                "Trascina qui PDF, PNG, PostScript, TIFF, JPEG o HP-GL/2",
                stile(RICHIAMO).resolve(ui.style()),
                Color32::from_gray(110),
            );
            return;
        };

        if self.su_foglio() {
            self.anteprima_foglio(ui, area, d);
            return;
        }

        // Il rotolo occupa la larghezza disponibile; le pagine, ognuna col suo
        // formato, sono in scala una sotto l'altra.
        let margine = 30.0;
        let spazio_tra_pagine = 8.0;
        let rotolo_mm = self.rotolo_effettivo();
        let scala = (area.width() - 2.0 * margine) / rotolo_mm as f32;
        let pagine_visibili = d.info.pagine.min(PAGINE_MAX);
        // Misure come escono dalla stampante: la T2600 ruota di 90° da sola
        // quando conviene (vedi `rotazione.rs`), scambiando i due lati. Le
        // pagine impaginate da IQClock arrivano già ruotate e larghe quanto il
        // rotolo: la stampante le lascia come sono.
        let misure: Vec<(f64, f64, bool)> = (0..pagine_visibili)
            .map(|n| {
                let (l, a) = d.mm_pagina(n).unwrap_or((rotolo_mm * 0.7, rotolo_mm));
                let ruotata = d.ruotata(n, rotolo_mm);
                if ruotata { (a, l, true) } else { (l, a, false) }
            })
            .collect();
        let ruotate_da_iqclock = d.pagine.iter().take(pagine_visibili).any(|p| p.ruotata);
        let mut nota = format!("{} — {}", d.nome, d.info.dettagli);
        if d.info.pagine > pagine_visibili {
            nota.push_str(&format!(" (mostrate {pagine_visibili})"));
        }
        let tolleranza = rotolo_mm + TOLLERANZA_MM;
        if misure.iter().any(|(l, a, _)| l.min(*a) > tolleranza) {
            nota.push_str("\nAttenzione: una pagina non entra nel rotolo, la stampante la ridimensionerà o taglierà");
        } else if misure.iter().any(|(_, a, r)| *r && *a > tolleranza) {
            nota.push_str("\nLa stampante ruoterà di 90° le pagine segnate, per farle entrare nel rotolo");
        } else if misure.iter().any(|(_, _, r)| *r) {
            nota.push_str("\nLa stampante ruoterà di 90° le pagine segnate, per risparmiare carta");
        }
        if ruotate_da_iqclock {
            nota.push_str("\nIQClock ha ruotato di 90° le pagine segnate prima di allinearle, come farebbe la stampante");
        }
        if let Some(dpi) = d.dpi
            && let Some(avviso) = dpi.avviso(self.config.soglia_dpi)
        {
            nota.push_str(&format!("\n{avviso}"));
        }
        // Anche senza un DPI da mostrare, la misura ferma alle prime pagine va
        // detta: il resto del documento non è stato guardato.
        if d.dpi_parziale {
            nota.push_str(&format!("\nRisoluzione misurata solo sulle prime {PAGINE_DPI_MAX} pagine"));
        }
        // Didascalia centrata che va a capo: con nomi lunghi o avvisi non
        // starebbe nella larghezza dell'anteprima.
        let mut testo = egui::text::LayoutJob::simple(nota, TextStyle::Body.resolve(ui.style()), Color32::from_gray(60), area.width() - 2.0 * margine);
        testo.halign = Align::Center;
        let nota = ui.painter().layout_job(testo);

        let lunghezza: f32 = misure.iter().map(|(_, a, _)| *a as f32 * scala + spazio_tra_pagine).sum();
        let alto_disponibile = area.height() - 2.0 * margine - nota.size().y;
        // Se il rotolo è troppo lungo per la finestra riduco tutto in proporzione.
        let rid = (alto_disponibile / lunghezza).min(1.0);
        let rotolo = Rect::from_min_size(
            Pos2::new(area.center().x - rotolo_mm as f32 * scala * rid / 2.0, area.top() + margine),
            Vec2::new(rotolo_mm as f32 * scala * rid, lunghezza * rid),
        );
        let p = ui.painter();
        p.rect_filled(rotolo, 0.0, Color32::from_gray(250));
        p.rect_stroke(rotolo, 0.0, Stroke::new(1.0, Color32::from_gray(160)), StrokeKind::Outside);

        let mut y = rotolo.top();
        let piu_pagine = misure.len() > 1;
        for (n, (l, a, ruotata)) in misure.iter().enumerate() {
            // Divisione tra una pagina e la precedente, a metà dello spazio vuoto.
            if n > 0 {
                let y_riga = y - spazio_tra_pagine * rid / 2.0;
                p.extend(egui::Shape::dashed_line(
                    &[Pos2::new(rotolo.left(), y_riga), Pos2::new(rotolo.right(), y_riga)],
                    Stroke::new(1.0, Color32::from_gray(120)),
                    6.0,
                    4.0,
                ));
            }
            let dim = Vec2::new(*l as f32, *a as f32) * scala * rid;
            // Una pagina impaginata è più stretta del rotolo del margine sui due
            // lati: la stampante la mette dal margine sinistro.
            let margine = match (d.pagine.get(n), self.impaginazione_anteprime) {
                (Some(p), Some(imp)) if p.impaginata => imp.margine_mm as f32 * scala * rid,
                _ => 0.0,
            };
            let r = Rect::from_min_size(Pos2::new(rotolo.left() + margine, y), dim);
            disegna_pagina(ui, r, d, n, *ruotata);
            if *ruotata || d.pagine.get(n).is_some_and(|p| p.ruotata) {
                p.text(r.left_top() + Vec2::new(5.0, 4.0), Align2::LEFT_TOP, "90°", TextStyle::Small.resolve(ui.style()), Color32::from_gray(90));
            }
            if piu_pagine {
                // A destra del rotolo, fuori dalle pagine: non copre il contenuto.
                p.text(Pos2::new(rotolo.right() + 4.0, r.top()), Align2::LEFT_TOP, (n + 1).to_string(), TextStyle::Small.resolve(ui.style()), Color32::from_gray(90));
            }
            y += dim.y + spazio_tra_pagine * rid;
        }

        let alto_nota = nota.size().y;
        p.galley(Pos2::new(area.center().x, area.bottom() - 8.0 - alto_nota), nota, Color32::from_gray(60));
    }

    /// Con il foglio singolo l'anteprima disegna i fogli al posto del rotolo:
    /// una pagina per foglio, tagliata sul bordo se non ci entra.
    fn anteprima_foglio(&self, ui: &mut egui::Ui, area: Rect, d: &Documento) {
        let foglio = self.foglio;
        let margine = 30.0;
        let spazio_tra_fogli = 14.0;
        // Senza pagine renderizzate resta il foglio vuoto: si vede almeno il
        // formato su cui uscirà.
        let pagine_visibili = d.info.pagine.clamp(1, PAGINE_MAX);
        let misure: Vec<Option<(f64, f64)>> =
            (0..pagine_visibili).map(|n| d.mm_pagina(n)).collect();
        let mut nota = format!(
            "{} — {}\n{} dall'ingresso manuale",
            d.nome,
            d.info.dettagli,
            foglio.etichetta()
        );
        if d.info.pagine > pagine_visibili {
            nota.push_str(&format!(" (mostrate {pagine_visibili})"));
        }
        if self.adatta_foglio && d.adattabile_al_foglio() {
            let percentuali: Vec<String> = (0..pagine_visibili)
                .map(|n| {
                    let base = d.info.pagine_mm.get(n).copied().flatten().or(d.info.mm);
                    match base {
                        Some((l, a)) => format!("p. {}: {:.1}%", n + 1, 100.0 * d.lavorazione.riduzione_foglio(l, a)),
                        None => format!("p. {}: misura ignota", n + 1),
                    }
                })
                .collect();
            nota.push_str(&format!("\nAdattamento rispetto alla scala scelta — {}", percentuali.join("; ")));
        } else if self.adatta_foglio {
            nota.push_str("\nAdattamento non supportato per questo documento");
        }
        // La stampante non stampa una pagina sul foglio della stessa misura:
        // vuole 5 mm ai lati e 22 mm sul lato d'entrata, e finché non li ha
        // chiede di caricare un foglio più grande (visto sulla T2600).
        if let Some((l, a)) = misure.iter().flatten().copied().find(|&m| !foglio.entra(m)) {
            let (min_l, min_a) = Foglio::foglio_minimo((l, a));
            nota.push_str(&format!(
                "\nAttenzione: la pagina {l:.0} × {a:.0} mm vuole un foglio di almeno {min_l:.0} × {min_a:.0} mm \
                 (22 mm sul lato d'entrata e 5 mm ai lati): la stampante chiederà di caricarlo"
            ));
        }
        if let Some(dpi) = d.dpi
            && let Some(avviso) = dpi.avviso(self.config.soglia_dpi)
        {
            nota.push_str(&format!("\n{avviso}"));
        }
        if d.dpi_parziale {
            nota.push_str(&format!(
                "\nRisoluzione misurata solo sulle prime {PAGINE_DPI_MAX} pagine"
            ));
        }
        let mut testo = egui::text::LayoutJob::simple(
            nota,
            TextStyle::Body.resolve(ui.style()),
            Color32::from_gray(60),
            area.width() - 2.0 * margine,
        );
        testo.halign = Align::Center;
        let nota = ui.painter().layout_job(testo);

        // Stessa scala per tutti i fogli: la larghezza disponibile, poi si
        // riduce tutto se la colonna non entra in altezza.
        let n = pagine_visibili as f32;
        let scala = (area.width() - 2.0 * margine) / foglio.larghezza_mm as f32;
        let altezza = n * foglio.altezza_mm as f32 * scala + (n - 1.0) * spazio_tra_fogli;
        let alto_disponibile = area.height() - 2.0 * margine - nota.size().y;
        let rid = (alto_disponibile / altezza).min(1.0);
        let larghezza = foglio.larghezza_mm as f32 * scala * rid;
        let altezza_foglio = foglio.altezza_mm as f32 * scala * rid;
        let mut y = area.top() + margine;
        for (i, misura) in misure.iter().enumerate() {
            let r = Rect::from_min_size(
                Pos2::new(area.center().x - larghezza / 2.0, y),
                Vec2::new(larghezza, altezza_foglio),
            );
            {
                // La pagina si disegna dentro il foglio, tagliata sul bordo:
                // quello che sborda non uscirebbe.
                let mut figlio = ui.new_child(egui::UiBuilder::new().max_rect(r));
                figlio.set_clip_rect(r);
                figlio.painter().rect_filled(r, 0.0, Color32::from_gray(250));
                if let Some((l, a)) = misura {
                    // Una pagina girata rispetto al foglio si mostra girata:
                    // sul foglio la rotazione della stampante non è provata.
                    let ruotata = (l > a) != (foglio.larghezza_mm > foglio.altezza_mm);
                    let dim = if ruotata {
                        Vec2::new(*a as f32, *l as f32)
                    } else {
                        Vec2::new(*l as f32, *a as f32)
                    } * (scala * rid);
                    disegna_pagina(&figlio, Rect::from_center_size(r.center(), dim), d, i, ruotata);
                }
            }
            let p = ui.painter();
            p.rect_stroke(
                r,
                0.0,
                Stroke::new(1.0, Color32::from_gray(160)),
                StrokeKind::Outside,
            );
            if pagine_visibili > 1 {
                p.text(
                    Pos2::new(r.right() + 4.0, r.top()),
                    Align2::LEFT_TOP,
                    (i + 1).to_string(),
                    TextStyle::Small.resolve(ui.style()),
                    Color32::from_gray(90),
                );
            }
            y += altezza_foglio + spazio_tra_fogli * rid;
        }

        let alto_nota = nota.size().y;
        ui.painter().galley(
            Pos2::new(area.center().x, area.bottom() - 8.0 - alto_nota),
            nota,
            Color32::from_gray(60),
        );
    }

    /// Con il nesting l'anteprima mostra i fogli di tutta la coda: basta
    /// `disponi`, che è veloce, non serve comporre il PDF vero.
    fn anteprima_nesting(&mut self, ui: &egui::Ui, area: Rect) {
        let rotolo_mm = self.rotolo_effettivo();
        let utile = (rotolo_mm - 2.0 * self.config.margine_mm).max(1.0);
        let mut pezzi = Vec::new();
        let mut senza = 0.0;
        let mut singoli = 0;
        for d in self.documenti.iter().filter(|d| matches!(d.stato, StatoJob::Pronto | StatoJob::Errore(_))) {
            if !d.annidabile {
                singoli += 1;
                continue;
            }
            for n in 0..d.info.pagine {
                let Some((l, a)) = d.mm_pagina(n) else { continue };
                for copia in 0..d.copie.max(1) {
                    pezzi.push(nesting::Pezzo { id: (d.id, n, copia), l, a });
                    senza += if rotazione::ruota(l, a, rotolo_mm) { l } else { a };
                }
            }
        }
        if pezzi.is_empty() {
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                if singoli > 0 {
                    format!("{singoli} documenti non annidabili: partono da soli")
                } else {
                    "Aggiungi PDF o immagini per vedere il nesting".to_string()
                },
                stile(RICHIAMO).resolve(ui.style()),
                Color32::from_gray(110),
            );
            return;
        }
        // Come `componi_nesting`: basta un pezzo fuori dal rotolo per mandare da
        // solo tutto il documento, così l'anteprima mostra i fogli che partiranno
        // davvero e non una disposizione che la stampa non farà.
        let fuori: HashSet<u64> = pezzi
            .iter()
            .filter(|p| !nesting::entra(p, utile, self.nesting_ruota, nesting::ALTEZZA_MAX_MM))
            .map(|p| p.id.0)
            .collect();
        // `senza` comprende anche questi documenti: il loro consumo singolo
        // va contato pure con il nesting, per confrontare la stessa coda.
        let altezza_esclusi: f64 = pezzi.iter().filter(|p| fuori.contains(&p.id.0)).map(|p| if rotazione::ruota(p.l, p.a, rotolo_mm) { p.l } else { p.a }).sum();
        pezzi.retain(|p| !fuori.contains(&p.id.0));
        if pezzi.is_empty() {
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                format!(
                    "Nessun pezzo entra nel rotolo da {rotolo_mm:.0} mm\n{} documenti partono da soli",
                    fuori.len() + singoli
                ),
                stile(RICHIAMO).resolve(ui.style()),
                Color32::from_gray(110),
            );
            return;
        }
        let opzioni = Nesting {
            distanza_mm: self.config.nesting_distanza_mm,
            ruota: self.nesting_ruota,
        };
        if !self.cache_nesting.as_ref().is_some_and(|cache| {
            cache.pezzi == pezzi && cache.utile == utile && cache.opzioni == opzioni
        }) {
            let fogli = nesting::disponi(
                &pezzi, utile, opzioni.distanza_mm, opzioni.ruota, nesting::ALTEZZA_MAX_MM,
            );
            self.cache_nesting = Some(CacheNesting { pezzi, utile, opzioni, fogli });
        }
        let fogli = &self.cache_nesting.as_ref().unwrap().fogli;
        let documenti: HashMap<_, _> = self.documenti.iter().map(|d| (d.id, d)).collect();
        let altezza_fogli: f64 = fogli.iter().map(|f| f.altezza).sum();
        let con = altezza_fogli + altezza_esclusi;
        let disposti: usize = fogli.iter().map(|f| f.posti.len()).sum();
        let spazio = 6.0f32;
        let totale = altezza_fogli as f32 + spazio * fogli.len().saturating_sub(1) as f32;
        let margine_px = 30.0;
        let mut nota = format!(
            "{} pezzi, {:.2} m di carta (invece di {:.2} m senza nesting)",
            disposti,
            con / 1000.0,
            senza / 1000.0
        );
        if singoli > 0 {
            nota.push_str(&format!("\n{singoli} documenti non annidabili: partono da soli"));
        }
        if !fuori.is_empty() {
            nota.push_str(&format!(
                "\n{} documenti non entrano nel rotolo: partono da soli",
                fuori.len()
            ));
        }
        let mut testo = egui::text::LayoutJob::simple(
            nota,
            TextStyle::Body.resolve(ui.style()),
            Color32::from_gray(60),
            area.width() - 2.0 * margine_px,
        );
        testo.halign = Align::Center;
        let nota = ui.painter().layout_job(testo);
        let scala = ((area.width() - 2.0 * margine_px) / rotolo_mm as f32)
            .min((area.height() - 2.0 * margine_px - nota.size().y) / totale.max(1.0))
            .max(0.0);
        let p = ui.painter();
        let rotolo = Rect::from_min_size(
            Pos2::new(
                area.center().x - rotolo_mm as f32 * scala / 2.0,
                area.top() + margine_px,
            ),
            Vec2::new(rotolo_mm as f32 * scala, totale * scala),
        );
        p.rect_filled(rotolo, 0.0, Color32::from_gray(250));
        p.rect_stroke(rotolo, 0.0, Stroke::new(1.0, Color32::from_gray(160)), StrokeKind::Outside);
        let sinistra = rotolo.left() + self.config.margine_mm as f32 * scala;
        let mut y = rotolo.top();
        let mut da_selezionare = None;
        for (i, foglio) in fogli.iter().enumerate() {
            if i > 0 {
                p.extend(egui::Shape::dashed_line(
                    &[
                        Pos2::new(rotolo.left(), y - spazio * scala / 2.0),
                        Pos2::new(rotolo.right(), y - spazio * scala / 2.0),
                    ],
                    Stroke::new(1.0, Color32::from_gray(120)),
                    6.0,
                    4.0,
                ));
            }
            for posto in &foglio.posti {
                let Some(d) = documenti.get(&posto.id.0) else { continue };
                let Some((l, a)) = d.mm_pagina(posto.id.1) else { continue };
                let (pl, pa) = if posto.ruotato { (a, l) } else { (l, a) };
                let r = Rect::from_min_size(
                    Pos2::new(sinistra + posto.x as f32 * scala, y + posto.y as f32 * scala),
                    Vec2::new(pl as f32 * scala, pa as f32 * scala),
                );
                disegna_pagina(ui, r, d, posto.id.1, posto.ruotato);
                // Copie del pezzo: quale copia è e quante ne sono in tutto, così
                // si vede nell'anteprima cosa moltiplica il numero scelto nella
                // sezione NESTING (o in COPIE).
                let copie = d.copie.max(1);
                if copie > 1 {
                    p.text(
                        Pos2::new(r.right() - 3.0, r.top() + 3.0),
                        Align2::RIGHT_TOP,
                        format!("{}/{}", posto.id.2 + 1, copie),
                        TextStyle::Small.resolve(ui.style()),
                        Color32::from_gray(80),
                    );
                }
                if self.selezionato.and_then(|i| self.documenti.get(i)).is_some_and(|s| s.id == d.id) {
                    p.rect_stroke(r, 0.0, Stroke::new(1.5, BLU_HP), StrokeKind::Outside);
                }
                // Click su un pezzo: si seleziona il file, così le copie si
                // cambiano dal suo pannello guardando i fogli.
                let risposta = ui.interact(
                    r,
                    ui.make_persistent_id(("nesting", posto.id.0, posto.id.1, posto.id.2)),
                    Sense::click(),
                );
                if risposta.clicked() {
                    da_selezionare = Some(d.id);
                }
                risposta.on_hover_text(format!("{} · copia {} di {}", d.nome, posto.id.2 + 1, copie));
            }
            y += foglio.altezza as f32 * scala + spazio * scala;
        }
        if let Some(id) = da_selezionare {
            self.selezionato = self.documenti.iter().position(|d| d.id == id);
        }
        p.galley(
            Pos2::new(area.center().x, area.bottom() - 8.0 - nota.size().y),
            nota,
            Color32::from_gray(60),
        );
    }
}

/// Riga "Magenta ▮▮▮▯ 13%" con barra colorata.
fn barra_inchiostro(ui: &mut egui::Ui, c: &Cartuccia) {
    let colore = match c.colore {
        Some("Ciano") => Color32::from_rgb(0x00, 0xA0, 0xE0),
        Some("Magenta") => Color32::from_rgb(0xE0, 0x00, 0x80),
        Some("Giallo") => Color32::from_rgb(0xF0, 0xD0, 0x00),
        Some("Grigio") => Color32::GRAY,
        _ => Color32::from_gray(30),
    };
    ui.horizontal(|ui| {
        let nome = c.colore.unwrap_or(&c.nome);
        let dim = Vec2::new(120.0, ui.text_style_height(&TextStyle::Body));
        ui.allocate_ui_with_layout(dim, Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_width(dim.x);
            ui.add(egui::Label::new(nome).truncate()).on_hover_text(&c.nome);
        });
        let (r, _) = ui.allocate_exact_size(Vec2::new(70.0, 10.0), Sense::hover());
        ui.painter().rect_filled(r, 2.0, Color32::from_gray(225));
        if let Some(p) = c.percentuale {
            let pieno = Rect::from_min_size(r.min, Vec2::new(r.width() * p as f32 / 100.0, r.height()));
            ui.painter().rect_filled(pieno, 2.0, colore);
            let testo = RichText::new(format!("{p}%"));
            ui.label(if p < 15 { testo.color(ARANCIO).strong() } else { testo });
        }
    });
}

/// Pagina `n` del documento: l'immagine renderizzata se c'è, altrimenti una
/// pagina bianca con il formato, centrata in `spazio` con le sue proporzioni.
/// Con `ruotata` la pagina è girata di 90°, come la stampa la stampante.
fn disegna_pagina(ui: &egui::Ui, spazio: Rect, d: &Documento, n: usize, ruotata: bool) {
    let tex = d.pagine.get(n).map(|p| &p.tex);
    let rapporto = match tex {
        Some(t) => t.aspect_ratio(),
        None => d.mm_pagina(n).map(|(l, a)| (l / a) as f32).unwrap_or(210.0 / 297.0),
    };
    let rapporto = if ruotata { 1.0 / rapporto } else { rapporto };
    let dim = if spazio.width() / spazio.height() > rapporto {
        Vec2::new(spazio.height() * rapporto, spazio.height())
    } else {
        Vec2::new(spazio.width(), spazio.width() / rapporto)
    };
    let r = Rect::from_center_size(spazio.center(), dim);
    let p = ui.painter();
    p.rect_filled(r, 0.0, Color32::WHITE);
    p.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_gray(150)), StrokeKind::Inside);
    if let Some(tex) = tex {
        if ruotata {
            // L'immagine si disegna dritta in un rettangolo coi lati scambiati
            // e si gira di 90° attorno al centro.
            egui::Image::from_texture(SizedTexture::from_handle(tex))
                .rotate(std::f32::consts::FRAC_PI_2, Vec2::splat(0.5))
                .paint_at(ui, Rect::from_center_size(r.center(), Vec2::new(r.height(), r.width())));
        } else {
            p.image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
    } else {
        p.text(r.center(), Align2::CENTER_CENTER, d.info.formato.etichetta(), FontId::proportional((r.height() / 6.0).clamp(9.0, 28.0)), Color32::from_gray(150));
    }
}

impl App {
    /// Finestra che chiede di confermare la larghezza del rotolo prima di
    /// mandare pagine impaginate.
    fn conferma_rotolo(&mut self, ctx: &egui::Context) {
        let mut scelta = None;
        let finestra = egui::Modal::new(egui::Id::unique("conferma_rotolo")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading("Larghezza del rotolo");
            ui.add_space(6.0);
            let testo = if self.nesting {
                format!(
                    "La stampante non ha comunicato la larghezza del rotolo: IQClock compone i fogli \
                     su {:.0} mm, scelti a mano. Se è sbagliata i pezzi non entrano o la stampante li taglia.",
                    self.larghezza_rotolo
                )
            } else {
                format!(
                    "La stampante non ha comunicato la larghezza del rotolo: IQClock allinea le pagine su \
                     {:.0} mm, scelti a mano. Se è sbagliata, la stampante le ridurrà o le taglierà.",
                    self.larghezza_rotolo
                )
            };
            ui.label(testo);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(format!("Stampa su {:.0} mm", self.larghezza_rotolo)).clicked() {
                    scelta = Some(true);
                }
                if ui.button("Annulla").clicked() {
                    scelta = Some(false);
                }
            });
        });
        if finestra.should_close() && scelta.is_none() {
            scelta = Some(false);
        }
        match scelta {
            Some(true) => {
                self.chiedi_conferma = false;
                self.rotolo_confermato = Some(self.larghezza_rotolo);
                self.stampa(ctx);
            }
            Some(false) => self.chiedi_conferma = false,
            None => {}
        }
    }

    fn foto_finestra(&mut self, ctx: &egui::Context) {
        let Some((percorso, fotogrammi)) = &mut self.foto else { return };
        *fotogrammi += 1;
        // Lascia il tempo a miniature e stato stampante di arrivare.
        if *fotogrammi < 120 {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
            return;
        }
        let percorso = percorso.clone();
        self.foto = None;
        ctx.request_screenshot(move |img| {
            let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            let [l, a] = img.size;
            if let Err(e) = image::save_buffer(&percorso, &rgba, l as u32, a as u32, image::ColorType::Rgba8) {
                eprintln!("foto non salvata: {e}");
            }
            std::process::exit(0);
        });
        ctx.request_repaint();
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let intervallo = std::time::Duration::from_secs(1);
        if self.ultimo_controllo_file.elapsed() >= intervallo {
            self.aggiorna_file_documenti();
        }
        ctx.request_repaint_after(intervallo);
        // Prima le chiavi aggiornate, poi i risultati: così quelli superati si scartano.
        self.file_trascinati(ctx);
        self.aggiorna_anteprime();
        self.gestisci_eventi(ctx);
        self.aggiorna_attesa_foglio(ctx);
        self.foto_finestra(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let sta_trascinando = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());

        egui::Panel::left("impostazioni").exact_size(260.0).show(ui, |ui| self.pannello_impostazioni(ui));
        egui::Panel::left("miniature").exact_size(170.0).show(ui, |ui| self.pannello_miniature(ui));
        if !self.avvisi.is_empty() {
            egui::Panel::bottom("avvisi").show(ui, |ui| {
                for a in &self.avvisi {
                    ui.colored_label(Color32::from_rgb(0xB0, 0x30, 0x30), a);
                }
                if ui.small_button("Chiudi").clicked() {
                    self.avvisi.clear();
                }
            });
        }
        egui::CentralPanel::default_margins().show(ui, |ui| self.anteprima(ui));
        if self.chiedi_conferma {
            self.conferma_rotolo(ui.ctx());
        }

        if sta_trascinando {
            let schermo = ui.ctx().content_rect();
            let p = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop")));
            p.rect_filled(schermo, 0.0, Color32::from_rgba_unmultiplied(0, 125, 186, 60));
            p.text(schermo.center(), Align2::CENTER_CENTER, "Rilascia per aggiungere", stile(RILASCIO).resolve(ui.style()), Color32::WHITE);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.config.larghezza_rotolo_mm = self.larghezza_rotolo;
        self.config.nesting = self.nesting;
        self.config.nesting_ruota = self.nesting_ruota;
        self.config.salva();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Un TIFF sovrascritto rinnova id, misure e idoneità, ma conserva le
    /// impostazioni dell'utente. Un file sparito non conserva misure obsolete.
    #[test]
    fn documento_rilegge_il_tiff_modificato() {
        let file = file::FileTemporaneo::nuovo("tiff");
        let scrivi = |pagine, larghezza, altezza| {
            let mut encoder = tiff::encoder::TiffEncoder::new(std::fs::File::create(file.percorso()).unwrap()).unwrap();
            for _ in 0..pagine {
                encoder.write_image::<tiff::encoder::colortype::RGB8>(larghezza, altezza, &vec![100; larghezza as usize * altezza as usize * 3]).unwrap();
            }
        };
        scrivi(1, 100, 200);
        let lav = Lavorazione { scala: Scala::Percentuale { x: 150.0, y: 150.0 }, ..Default::default() };
        let mut d = Documento {
            id: 1,
            percorso: file.percorso().to_path_buf(),
            nome: "modificato.tiff".into(),
            info: file::analizza(file.percorso()).unwrap(),
            annidabile: true,
            impronta: impronta_file(file.percorso()),
            copie: 3,
            stato: StatoJob::Pronto,
            lavorazione: lav,
            blocca_proporzioni: true,
            anteprima: (None, lav),
            pagine: Vec::new(),
            dpi: None,
            dpi_parziale: false,
        };
        let prima = d.info.mm;
        assert!(!d.aggiorna_file(2));
        scrivi(2, 80, 90);
        assert!(d.aggiorna_file(2));
        assert!(!d.annidabile);
        assert_ne!(d.info.mm, prima);
        assert_eq!(d.id, 2);
        assert_eq!(d.copie, 3);
        assert_eq!(d.lavorazione, lav);
        assert!(!d.aggiorna_file(3));
        std::fs::remove_file(file.percorso()).unwrap();
        assert!(d.aggiorna_file(3));
        assert!(!d.annidabile);
        assert!(d.mm_pagina(0).is_none());
        scrivi(1, 120, 160);
        assert!(d.aggiorna_file(4));
        assert!(d.annidabile);
        assert!(d.mm_pagina(0).is_some());
    }
}
