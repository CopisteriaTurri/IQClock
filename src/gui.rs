//! Interfaccia in stile HP Click: impostazioni a sinistra, miniature al centro,
//! anteprima sul rotolo a destra, file trascinabili ovunque nella finestra.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use eframe::egui::load::SizedTexture;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, TextStyle, TextureHandle, TextureOptions, Vec2,
};

use crate::anteprima::{Anteprime, PAGINE_MAX, Pagina};
use crate::config::Config;
use crate::file::{self, InfoFile};
use crate::impagina::{self, Allineamento, Impaginazione};
use crate::pjl::{self, OpzioniJob, Sorgente};
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
    copie: u32,
    stato: StatoJob,
    /// Prime pagine renderizzate (vuoto finché non arrivano, o se l'anteprima
    /// non è disponibile).
    pagine: Vec<PaginaMostrata>,
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
    /// Misure della pagina `n` in mm: quelle date dall'anteprima, altrimenti
    /// quelle della prima pagina lette all'apertura del file.
    fn mm_pagina(&self, n: usize) -> Option<(f64, f64)> {
        self.pagine.get(n).and_then(|p| p.mm).or(self.info.mm)
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

enum Evento {
    Avanzamento(u64, f32),
    Finito(u64, Result<(), String>),
    Stato(Result<StatoStampante, String>),
    Pagine(u64, Vec<Pagina>),
}

pub struct App {
    config: Config,
    documenti: Vec<Documento>,
    prossimo_id: u64,
    anteprime: Anteprime,
    selezionato: Option<usize>,
    sorgente: Sorgente,
    scala_di_grigi: bool,
    taglierina: Option<bool>,
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
    avvisi: Vec<String>,
    /// Con `IQCLOCK_FOTO=file.png` salva un'immagine della finestra e chiude
    /// (per documentazione e controlli visivi).
    foto: Option<(PathBuf, u32)>,
    tx: Sender<Evento>,
    rx: Receiver<Evento>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config, iniziali: Vec<PathBuf>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        configura_stile(&cc.egui_ctx);
        let (tx, rx) = channel();
        let (tx_pagine, ctx_pagine) = (tx.clone(), cc.egui_ctx.clone());
        let (anteprime, errore_pdfium) = Anteprime::avvia(move |id, pagine| {
            let _ = tx_pagine.send(Evento::Pagine(id, pagine));
            ctx_pagine.request_repaint();
        });
        let mut app = Self {
            larghezza_rotolo: config.larghezza_rotolo_mm,
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
            scala_di_grigi: false,
            taglierina: None,
            extra_pjl: String::new(),
            larghezza_rilevata: None,
            stato_stampante: None,
            verifica_in_corso: false,
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

    fn aggiungi(&mut self, percorso: PathBuf) {
        let nome = percorso.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match file::analizza(&percorso) {
            Ok(info) => {
                let id = self.prossimo_id;
                self.prossimo_id += 1;
                self.anteprime.richiedi(id, percorso.clone(), info.formato, self.impaginazione_anteprime);
                self.documenti.push(Documento {
                    id,
                    percorso,
                    nome,
                    info,
                    copie: 1,
                    stato: StatoJob::Pronto,
                    pagine: Vec::new(),
                });
                self.selezionato = Some(self.documenti.len() - 1);
            }
            Err(e) => self.avvisi.push(format!("{nome}: {e}")),
        }
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

    fn impaginazione(&self) -> Option<Impaginazione> {
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

    /// Richiede di nuovo le anteprime quando cambia l'impaginazione: mostrano
    /// il file che parte davvero.
    fn aggiorna_anteprime(&mut self) {
        let imp = self.impaginazione();
        if imp == self.impaginazione_anteprime {
            return;
        }
        self.impaginazione_anteprime = imp;
        for d in &self.documenti {
            if impagina::impaginabile(d.info.formato) {
                self.anteprime.richiedi(d.id, d.percorso.clone(), d.info.formato, imp);
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
        }
    }

    /// Prima di impaginare con una larghezza del rotolo non verificata chiede
    /// conferma.
    fn chiedi_e_stampa(&mut self, ctx: &egui::Context) {
        let da_impaginare = self.impaginazione().is_some()
            && self.documenti.iter().any(|d| {
                matches!(d.stato, StatoJob::Pronto | StatoJob::Errore(_)) && impagina::impaginabile(d.info.formato)
            });
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
                Evento::Finito(id, Ok(())) => self.imposta_stato(id, StatoJob::Inviato),
                Evento::Finito(id, Err(e)) => self.imposta_stato(id, StatoJob::Errore(e)),
                Evento::Stato(r) => {
                    if let Ok(s) = &r {
                        self.larghezza_rilevata =
                            s.larghezza_rotolo_mm().map(|(n, m)| (n.to_string(), m));
                    } else {
                        self.larghezza_rilevata = None;
                    }
                    self.stato_stampante = Some(r);
                    self.verifica_in_corso = false;
                }
                Evento::Pagine(id, pagine) => {
                    if let Some(d) = self.documenti.iter_mut().find(|d| d.id == id) {
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

    fn pannello_impostazioni(&mut self, ui: &mut egui::Ui) {
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
                    Some(Ok(_)) => "Rotolo non rilevato (la MIB non espone le dimensioni): uso il fallback manuale",
                    Some(Err(_)) | None => "Rotolo non rilevato: uso il fallback manuale",
                };
                ui.colored_label(Color32::GRAY, nota);
            }
        }

        ui.add_space(6.0);
        sezione(ui, "ALLINEAMENTO");
        let escluso = self
            .selezionato
            .and_then(|i| self.documenti.get(i))
            .is_some_and(|d| !impagina::impaginabile(d.info.formato));
        ui.add_enabled_ui(!escluso, |ui| {
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
        if self.allineamento != Allineamento::Sinistra {
            ui.horizontal(|ui| {
                ui.label("Margine");
                ui.add(egui::DragValue::new(&mut self.config.margine_mm).range(impagina::MARGINE_MIN_MM..=50.0).speed(0.5).suffix(" mm"))
            })
            .response
            .on_hover_text("Bordo non stampabile per lato: la pagina mandata è larga quanto il rotolo meno questo margine sui due lati");
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

        if let Some(i) = self.selezionato {
            ui.add_space(6.0);
            sezione(ui, "COPIE");
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
        egui::CollapsingHeader::new("Avanzate (PJL)").show(ui, |ui| {
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

    fn pannello_miniature(&mut self, ui: &mut egui::Ui) {
        let mut da_togliere = None;
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
                        if (0..d.info.pagine.min(PAGINE_MAX)).any(|n| d.esce_ruotata(n, self.rotolo_effettivo())) {
                            didascalia.push_str(" · 90°");
                        }
                        ui.label(RichText::new(didascalia).small());
                        if !d.info.formati_pagine.is_empty() {
                            ui.label(RichText::new(file::etichetta_formati(&d.info.formati_pagine)).small());
                        }
                        if let Some(r) = &d.info.risoluzione {
                            let testo = RichText::new(r.etichetta()).small();
                            ui.label(if r.presunta { testo.color(ARANCIO) } else { testo });
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

    fn anteprima(&self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, Color32::from_gray(225));
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
            ui.label(format!(
                "La stampante non ha comunicato la larghezza del rotolo: IQClock allinea le pagine su \
                 {:.0} mm, scelti a mano. Se è sbagliata, la stampante le ridurrà o le taglierà.",
                self.larghezza_rotolo
            ));
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
        self.gestisci_eventi(ctx);
        self.file_trascinati(ctx);
        self.aggiorna_anteprime();
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
        self.config.salva();
    }
}
