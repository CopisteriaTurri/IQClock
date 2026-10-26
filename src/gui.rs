//! Interfaccia in stile HP Click: impostazioni a sinistra, miniature al centro,
//! anteprima sul rotolo a destra, file trascinabili ovunque nella finestra.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2,
};

use crate::anteprima::{Anteprime, PAGINE_MAX, Pagina};
use crate::config::Config;
use crate::file::{self, InfoFile};
use crate::pjl::{self, OpzioniJob, Sorgente};
use crate::stampante;
use crate::stato::{self, Cartuccia, StatoStampante};

const BLU_HP: Color32 = Color32::from_rgb(0x00, 0x7D, 0xBA);
const ARANCIO: Color32 = Color32::from_rgb(0xC0, 0x60, 0x00);
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
    /// Prime pagine renderizzate con le loro misure in mm, se note
    /// (vuoto finché non arrivano, o se l'anteprima non è disponibile).
    pagine: Vec<(TextureHandle, Option<(f64, f64)>)>,
}

impl Documento {
    /// Misure della pagina `n` in mm: quelle date da pdfium, altrimenti quelle
    /// della prima pagina lette all'apertura del file.
    fn mm_pagina(&self, n: usize) -> Option<(f64, f64)> {
        self.pagine.get(n).and_then(|(_, mm)| *mm).or(self.info.mm)
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
        let (tx, rx) = channel();
        let (tx_pagine, ctx_pagine) = (tx.clone(), cc.egui_ctx.clone());
        let (anteprime, errore_pdfium) = Anteprime::avvia(move |id, pagine| {
            let _ = tx_pagine.send(Evento::Pagine(id, pagine));
            ctx_pagine.request_repaint();
        });
        let mut app = Self {
            larghezza_rotolo: config.larghezza_rotolo_mm,
            config,
            documenti: Vec::new(),
            prossimo_id: 0,
            anteprime,
            selezionato: None,
            sorgente: Sorgente::Stampante,
            scala_di_grigi: false,
            taglierina: None,
            extra_pjl: String::new(),
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
                self.anteprime.richiedi(id, percorso.clone(), info.formato);
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

    fn opzioni(&self, doc: &Documento) -> OpzioniJob {
        OpzioniJob {
            nome: doc.nome.clone(),
            copie: doc.copie,
            sorgente: self.sorgente,
            scala_di_grigi: self.scala_di_grigi,
            taglierina: self.taglierina,
            extra: self.extra_pjl.clone(),
        }
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
                                (tex, p.mm)
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

        ui.label(RichText::new("STAMPANTE").small().strong());
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
                    ui.colored_label(colore, format!("{nome}: {}", i.descrizione()));
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
        ui.label(RichText::new("CARTA").small().strong());
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
            .on_hover_text("Larghezza del rotolo, usata solo per l'anteprima");

        ui.add_space(6.0);
        ui.label(RichText::new("COLORE").small().strong());
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.scala_di_grigi, false, "Colore");
            ui.selectable_value(&mut self.scala_di_grigi, true, "Scala di grigi");
        });

        ui.add_space(6.0);
        ui.label(RichText::new("TAGLIERINA").small().strong());
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.taglierina, None, "Stampante");
            ui.selectable_value(&mut self.taglierina, Some(true), "Sì");
            ui.selectable_value(&mut self.taglierina, Some(false), "No");
        })
        .response
        .on_hover_text("CUTTER non è nel manuale PJL: verificare che la T2600 lo rispetti");

        if let Some(i) = self.selezionato {
            ui.add_space(6.0);
            ui.label(RichText::new("COPIE").small().strong());
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
            let pulsante = egui::Button::new(RichText::new("Stampa").color(Color32::WHITE).size(16.0))
                .fill(BLU_HP)
                .corner_radius(CornerRadius::same(16))
                .min_size(Vec2::new(ui.available_width(), 36.0));
            if ui.add_enabled(pronti > 0, pulsante).clicked() {
                self.stampa(ui.ctx());
            }
            if pronti > 1 {
                ui.label(format!("{pronti} file da stampare"));
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
                        disegna_pagina(ui, rect, d, 0);
                        ui.label(RichText::new(&d.nome).small().strong());
                        ui.label(RichText::new(format!("{} · {}", d.info.formato.etichetta(), file::dimensione_leggibile(d.info.byte))).small());
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
                    .add_filter("Stampabili", &["pdf", "ps", "eps", "tif", "tiff", "jpg", "jpeg", "plt", "hpgl"])
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
                "Trascina qui PDF, PostScript, TIFF, JPEG o HP-GL/2",
                FontId::proportional(20.0),
                Color32::from_gray(110),
            );
            return;
        };

        // Il rotolo occupa la larghezza disponibile; le pagine, ognuna col suo
        // formato, sono in scala una sotto l'altra.
        let margine = 30.0;
        let spazio_tra_pagine = 8.0;
        let rotolo_mm = self.larghezza_rotolo;
        let scala = (area.width() - 2.0 * margine) / rotolo_mm as f32;
        let pagine_visibili = d.info.pagine.min(PAGINE_MAX);
        let misure: Vec<(f64, f64)> = (0..pagine_visibili)
            .map(|n| d.mm_pagina(n).unwrap_or((rotolo_mm * 0.7, rotolo_mm)))
            .collect();
        let lunghezza: f32 = misure.iter().map(|(_, a)| *a as f32 * scala + spazio_tra_pagine).sum();
        let alto_disponibile = area.height() - 2.0 * margine - 24.0;
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
        for (n, (l, a)) in misure.iter().enumerate() {
            let dim = Vec2::new(*l as f32, *a as f32) * scala * rid;
            disegna_pagina(ui, Rect::from_min_size(Pos2::new(rotolo.left(), y), dim), d, n);
            y += dim.y + spazio_tra_pagine * rid;
        }

        let mut nota = format!("{} — {}", d.nome, d.info.dettagli);
        if d.info.pagine > pagine_visibili {
            nota.push_str(&format!(" (mostrate {pagine_visibili})"));
        }
        let tolleranza = rotolo_mm + 0.5;
        if misure.iter().any(|(l, a)| l.min(*a) > tolleranza) {
            nota.push_str("  — attenzione: una pagina non entra nel rotolo, la stampante la ridimensionerà o taglierà");
        } else if misure.iter().any(|(l, _)| *l > tolleranza) {
            nota.push_str("  — attenzione: una pagina è più larga del rotolo, va ruotata (impostazione della stampante)");
        }
        ui.painter().text(
            Pos2::new(area.center().x, area.bottom() - 16.0),
            Align2::CENTER_CENTER,
            nota,
            FontId::proportional(13.0),
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
        ui.add_sized([100.0, 14.0], egui::Label::new(RichText::new(nome).small()).truncate())
            .on_hover_text(&c.nome);
        let (r, _) = ui.allocate_exact_size(Vec2::new(90.0, 8.0), Sense::hover());
        ui.painter().rect_filled(r, 2.0, Color32::from_gray(225));
        if let Some(p) = c.percentuale {
            let pieno = Rect::from_min_size(r.min, Vec2::new(r.width() * p as f32 / 100.0, r.height()));
            ui.painter().rect_filled(pieno, 2.0, colore);
            let testo = RichText::new(format!("{p}%")).small();
            ui.label(if p < 15 { testo.color(ARANCIO).strong() } else { testo });
        }
    });
}

/// Pagina `n` del documento: l'immagine renderizzata se c'è, altrimenti una
/// pagina bianca con il formato, centrata in `spazio` con le sue proporzioni.
fn disegna_pagina(ui: &egui::Ui, spazio: Rect, d: &Documento, n: usize) {
    let tex = d.pagine.get(n).map(|(t, _)| t);
    let rapporto = match tex {
        Some(t) => t.aspect_ratio(),
        None => d.mm_pagina(n).map(|(l, a)| (l / a) as f32).unwrap_or(210.0 / 297.0),
    };
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
        p.image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    } else {
        p.text(r.center(), Align2::CENTER_CENTER, d.info.formato.etichetta(), FontId::proportional((r.height() / 6.0).clamp(9.0, 28.0)), Color32::from_gray(150));
    }
}

impl App {
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

        if sta_trascinando {
            let schermo = ui.ctx().content_rect();
            let p = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop")));
            p.rect_filled(schermo, 0.0, Color32::from_rgba_unmultiplied(0, 125, 186, 60));
            p.text(schermo.center(), Align2::CENTER_CENTER, "Rilascia per aggiungere", FontId::proportional(28.0), Color32::WHITE);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.config.larghezza_rotolo_mm = self.larghezza_rotolo;
        self.config.salva();
    }
}
