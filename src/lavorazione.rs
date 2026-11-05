//! Trasformazioni scelte per un documento prima dell'impaginazione sul rotolo.
//!
//! Per ora c'è solo la scala (#10): originale, formato ISO A0–A4, percentuale o
//! misura in mm. I PDF si scalano in vettoriale (nuovo MediaBox e una matrice
//! `cm` attorno al contenuto, annotazioni comprese); le immagini non vengono
//! ricampionate, cambiano solo i DPI dichiarati. Con la sola scala JPEG e TIFF
//! partono nel loro formato, con i soli campi della risoluzione riscritti (vedi
//! `risoluzione.rs`). PostScript e HP-GL/2 partono sempre come sono.
//!
//! Poi la cornice di taglio (#9): una linea continua attorno al contenuto, per
//! cui la pagina cresce di `distanza + spessore` per lato. Sui PDF è un
//! rettangolo vettoriale in più, sulle immagini un bordo disegnato mentre si
//! scrive il TIFF (così non serve una copia in più in memoria).
//!
//! L'ordine dei passi è quello comune a tutte le issue: prima la scala, poi la
//! cornice, poi l'allineamento (qui rifattorizzato in `impagina::applica_pdf`):
//! così l'allineamento misura il pezzo con la sua cornice.

use std::path::Path;

use anyhow::{Context, Result, bail};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::file::{self, FileTemporaneo};
use crate::impagina::{self, Impaginazione};
use crate::pjl::Formato;
use crate::png;
use crate::risoluzione;

const MM_PT: f64 = 72.0 / 25.4;
/// Fattori ammessi: sotto l'1% o sopra il 1000% la pagina non è più utile.
const FATTORE_MIN: f64 = 0.01;
const FATTORE_MAX: f64 = 10.0;

/// Formati ISO A che si possono scegliere come destinazione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatoIso {
    A0,
    A1,
    A2,
    A3,
    A4,
}

impl FormatoIso {
    pub const TUTTI: [FormatoIso; 5] = [
        FormatoIso::A0,
        FormatoIso::A1,
        FormatoIso::A2,
        FormatoIso::A3,
        FormatoIso::A4,
    ];

    pub fn etichetta(self) -> &'static str {
        match self {
            FormatoIso::A0 => "A0",
            FormatoIso::A1 => "A1",
            FormatoIso::A2 => "A2",
            FormatoIso::A3 => "A3",
            FormatoIso::A4 => "A4",
        }
    }

    /// Lato corto e lato lungo in mm, dalle misure comuni di `file::iso`.
    pub fn mm(self) -> (f64, f64) {
        let nome = self.etichetta();
        let (_, corto, lungo) = file::ISO
            .iter()
            .find(|(n, ..)| *n == nome)
            .copied()
            .expect("formato ISO presente nella tabella");
        (corto, lungo)
    }

    pub fn da_testo(testo: &str) -> Option<Self> {
        Self::TUTTI
            .into_iter()
            .find(|f| f.etichetta().eq_ignore_ascii_case(testo))
    }
}

/// Scala di un documento. Con `Misura` il lock delle proporzioni viaggia nel
/// modello perché cambia i fattori; per la percentuale lo tiene la GUI.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Scala {
    #[default]
    Originale,
    /// Adatta la pagina al formato tenendo le proporzioni, nello stesso
    /// orientamento della pagina.
    Formato(FormatoIso),
    /// Percentuale della misura originale; con il lock `x == y`.
    Percentuale { x: f64, y: f64 },
    /// Misura finale in mm; con `proporzioni` la pagina ci sta dentro senza
    /// deformarsi, altrimenti i due assi si scalano separatamente.
    Misura {
        larghezza_mm: f64,
        altezza_mm: f64,
        proporzioni: bool,
    },
}

impl Scala {
    /// true se la scala non cambia niente a prescindere dalla pagina: l'originale
    /// e il 100 % (così un PDF che non si può scalare non fallisce per niente).
    pub fn originale(&self) -> bool {
        match *self {
            Scala::Originale => true,
            Scala::Percentuale { x, y } => x == 100.0 && y == 100.0,
            _ => false,
        }
    }

    /// Etichetta breve: "Originale", "A3", "50 %", "500 × 700 mm".
    pub fn etichetta(&self) -> String {
        match *self {
            Scala::Originale => "Originale".to_string(),
            Scala::Formato(f) => f.etichetta().to_string(),
            Scala::Percentuale { x, y } if (x - y).abs() < 0.05 => {
                format!("{} %", numero(x))
            }
            Scala::Percentuale { x, y } => format!("{} × {} %", numero(x), numero(y)),
            Scala::Misura {
                larghezza_mm,
                altezza_mm,
                ..
            } => {
                format!("{} × {} mm", numero(larghezza_mm), numero(altezza_mm))
            }
        }
    }

    /// Didascalia per la miniatura: "→ A3", "→ 50 %"; vuota se è l'originale.
    pub fn didascalia(&self) -> String {
        if self.originale() {
            String::new()
        } else {
            format!("→ {}", self.etichetta())
        }
    }

    /// Legge `A3`, `50%`, `500x700` (proporzioni bloccate) o `500x700!`
    /// (deformando). `None` se la sintassi o i valori non vanno bene.
    pub fn da_testo(testo: &str) -> Option<Self> {
        let testo = testo.trim();
        if testo.eq_ignore_ascii_case("originale") {
            return Some(Scala::Originale);
        }
        if let Some(f) = FormatoIso::da_testo(testo) {
            return Some(Scala::Formato(f));
        }
        if let Some(percento) = testo.strip_suffix('%') {
            let x: f64 = percento.trim().parse().ok()?;
            return (x.is_finite() && (1.0..=1000.0).contains(&x))
                .then_some(Scala::Percentuale { x, y: x });
        }
        let (misure, proporzioni) = match testo.strip_suffix('!') {
            Some(m) => (m, false),
            None => (testo, true),
        };
        let (larghezza, altezza) = misure.split_once(['x', 'X', '×'])?;
        let larghezza_mm: f64 = larghezza.trim().parse().ok()?;
        let altezza_mm: f64 = altezza.trim().parse().ok()?;
        (larghezza_mm.is_finite()
            && altezza_mm.is_finite()
            && larghezza_mm > 0.0
            && altezza_mm > 0.0)
            .then_some(Scala::Misura {
                larghezza_mm,
                altezza_mm,
                proporzioni,
            })
    }
}

/// Cornice di taglio attorno al contenuto: una linea continua **fuori** dal
/// contenuto, a `distanza_mm` da esso, spessa `spessore_mm`. Il contenuto resta
/// della sua misura e la pagina cresce di `2 × (distanza + spessore)` per lato,
/// così il bordo esterno della linea coincide con il bordo del foglio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cornice {
    pub distanza_mm: f64,
    pub spessore_mm: f64,
}

impl Default for Cornice {
    fn default() -> Self {
        Self { distanza_mm: 3.0, spessore_mm: 0.25 }
    }
}

impl Cornice {
    /// Spazio che la cornice aggiunge al pezzo, per lato (mm).
    pub fn ingombro_mm(&self) -> f64 {
        (self.distanza_mm + self.spessore_mm).max(0.0)
    }
}

/// Trasformazioni scelte per un documento.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Lavorazione {
    pub scala: Scala,
    /// Cornice di taglio attorno al contenuto (la accende il toggle di FINITURA
    /// nella GUI): viaggia per documento, così impaginazione e nesting la
    /// trattano come parte del pezzo.
    pub cornice: Option<Cornice>,
}

impl Lavorazione {
    pub fn nulla(&self) -> bool {
        self.scala.originale() && self.cornice.is_none()
    }
}

/// La scala si applica a PDF e immagini, non a PostScript e HP-GL/2.
pub fn lavorabile(formato: Formato) -> bool {
    !matches!(formato, Formato::PostScript | Formato::Hpgl2)
}

/// Fattori di scala per una pagina di `larghezza_mm` × `altezza_mm` (misure
/// visive, con `/Rotate` già applicato).
pub fn fattori(scala: &Scala, larghezza_mm: f64, altezza_mm: f64) -> (f64, f64) {
    let (sx, sy) = fattori_grezzi(scala, larghezza_mm, altezza_mm);
    (limita(sx), limita(sy))
}

/// true se per questa pagina la scala chiesta supera i limiti (1 %–1000 %) e
/// verrebbe ridotta in silenzio.
pub fn fuori_limiti(scala: &Scala, larghezza_mm: f64, altezza_mm: f64) -> bool {
    let (sx, sy) = fattori_grezzi(scala, larghezza_mm, altezza_mm);
    [sx, sy]
        .iter()
        .any(|&f| f.is_finite() && !(FATTORE_MIN * 0.9999..=FATTORE_MAX * 1.0001).contains(&f))
}

/// Misure minime e massime (mm) che la scala accetta per una pagina così grande.
pub fn limiti_mm(larghezza_mm: f64, altezza_mm: f64) -> ((f64, f64), (f64, f64)) {
    (
        ((larghezza_mm * FATTORE_MIN).max(1.0), (altezza_mm * FATTORE_MIN).max(1.0)),
        (larghezza_mm * FATTORE_MAX, altezza_mm * FATTORE_MAX),
    )
}

fn fattori_grezzi(scala: &Scala, larghezza_mm: f64, altezza_mm: f64) -> (f64, f64) {
    if !(larghezza_mm > 0.0 && altezza_mm > 0.0) {
        return (1.0, 1.0);
    }
    match *scala {
        Scala::Originale => (1.0, 1.0),
        Scala::Formato(formato) => {
            let (corto, lungo) = formato.mm();
            // Stesso orientamento della pagina: una A1 verticale verso A3 resta
            // verticale, una orizzontale usa il formato orizzontale.
            let (formato_l, formato_a) = if larghezza_mm > altezza_mm {
                (lungo, corto)
            } else {
                (corto, lungo)
            };
            let s = (formato_l / larghezza_mm).min(formato_a / altezza_mm);
            (s, s)
        }
        Scala::Percentuale { x, y } => (x / 100.0, y / 100.0),
        Scala::Misura {
            larghezza_mm: l,
            altezza_mm: a,
            proporzioni: true,
        } => {
            let s = (l / larghezza_mm).min(a / altezza_mm);
            (s, s)
        }
        Scala::Misura {
            larghezza_mm: l,
            altezza_mm: a,
            proporzioni: false,
        } => (l / larghezza_mm, a / altezza_mm),
    }
}

fn limita(fattore: f64) -> f64 {
    if fattore.is_finite() {
        fattore.clamp(FATTORE_MIN, FATTORE_MAX)
    } else {
        1.0
    }
}

fn numero(v: f64) -> String {
    if (v - v.round()).abs() < 0.05 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Prepara il file da mandare: scala e, se c'è, allineamento sul rotolo. `None`
/// se nessun passo cambia il file, che parte com'è.
pub fn prepara(
    percorso: &Path,
    formato: Formato,
    lav: &Lavorazione,
    imp: Option<&Impaginazione>,
) -> Result<Option<(FileTemporaneo, Formato)>> {
    match formato {
        Formato::Pdf => {
            if lav.nulla() && imp.is_none() {
                return Ok(None);
            }
            let dati = std::fs::read(percorso)?;
            let Some(composto) = componi_pdf(&dati, lav, imp)? else {
                return Ok(None);
            };
            let dest = FileTemporaneo::nuovo("pdf");
            std::fs::write(dest.percorso(), composto.dati)
                .context("scrittura del PDF ridimensionato")?;
            Ok(Some((dest, Formato::Pdf)))
        }
        Formato::Jpeg | Formato::Tiff | Formato::Png => {
            prepara_immagine(percorso, formato, lav, imp)
        }
        Formato::PostScript | Formato::Hpgl2 => Ok(None),
    }
}

/// Applica scala e allineamento a un PDF già letto. `None` se nessuna pagina
/// cambia. Con la sola scala originale passa dal percorso già collaudato di
/// `impagina`.
pub fn componi_pdf(
    dati: &[u8],
    lav: &Lavorazione,
    imp: Option<&Impaginazione>,
) -> Result<Option<impagina::PdfComposto>> {
    if lav.nulla() {
        return match imp {
            Some(imp) => impagina::componi_pdf(dati, imp),
            None => Ok(None),
        };
    }
    let mut doc = Document::load_mem(dati).context("apertura del PDF")?;
    if doc.is_encrypted() {
        // Le misure delle pagine sono numeri, non stringhe cifrate: se si
        // leggono anche senza password e la scala non cambierebbe niente (es.
        // "Formato A4" su un PDF già tutto A4) non c'è motivo di rifiutare il
        // documento. Oggi però lopdf, quando la password non c'è, si ferma
        // prima di leggere il resto del documento (`get_pages` vuoto): in quel
        // caso resta il rifiuto, come prima.
        let pagine: Vec<ObjectId> = doc.get_pages().into_values().collect();
        if pagine.is_empty() {
            bail!("PDF cifrato: non si può ridimensionare (pagine non leggibili); scegli originale");
        }
        let cambierebbero = pagine
            .iter()
            .filter(|&&id| scala_pagina(&doc, id, &lav.scala).is_some())
            .count();
        if cambierebbero > 0 {
            bail!(
                "PDF cifrato: non si può ridimensionare ({cambierebbero} pagine cambierebbero misura); scegli originale"
            );
        }
        return match imp {
            Some(imp) => impagina::componi_pdf(dati, imp),
            None => Ok(None),
        };
    }
    // Conserva la decisione prima di modificare box e contenuti: l'anteprima
    // deve misurare i DPI con la stessa scala che viene mandata in stampa.
    let fattori_scala = doc
        .get_pages()
        .into_values()
        .map(|id| scala_pagina(&doc, id, &lav.scala).map_or((1.0, 1.0), |p| p.fattori))
        .collect();
    let mut cambiato = applica_scala(&mut doc, lav)?;
    if let Some(cornice) = &lav.cornice {
        cambiato |= applica_cornice(&mut doc, cornice)?;
    }
    let posizioni = match imp {
        Some(imp) => {
            let (cambiate, posizioni) = impagina::applica_pdf(&mut doc, imp)?;
            cambiato |= cambiate;
            posizioni
        }
        None => vec![None; doc.get_pages().len()],
    };
    if !cambiato {
        return Ok(None);
    }
    let mut out = Vec::new();
    doc.save_to(&mut out)
        .context("scrittura del PDF ridimensionato")?;
    Ok(Some(impagina::PdfComposto {
        dati: out,
        posizioni,
        fattori_scala,
    }))
}

fn applica_scala(doc: &mut Document, lav: &Lavorazione) -> Result<bool> {
    if lav.scala.originale() {
        return Ok(false);
    }
    let mut cambiato = false;
    for id in doc.get_pages().into_values().collect::<Vec<_>>() {
        cambiato |= applica_scala_pagina(doc, id, &lav.scala)?;
    }
    Ok(cambiato)
}

/// Cornice su ogni pagina: allarga il box di `distanza + spessore` per lato e
/// disegna la linea nera sul bordo esterno. Va prima dell'allineamento, così
/// `posiziona` riceve le misure con la cornice (la cornice fa parte del pezzo).
/// Distanza e spessore sono convertiti nelle unità della singola pagina:
/// `/UserUnit` moltiplica la misura fisica dell'unità utente (default 1).
fn applica_cornice(doc: &mut Document, cornice: &Cornice) -> Result<bool> {
    let mut cambiato = false;
    for id in doc.get_pages().into_values().collect::<Vec<_>>() {
        let Some(([x0, y0, x1, y1], _)) = file::box_e_rotazione(doc, id) else {
            continue;
        };
        let unita = doc.get_dictionary(id)?.get(b"UserUnit").ok()
            .and_then(|valore| doc.dereference(valore).ok())
            .and_then(|(_, valore)| valore.as_float().ok())
            .map(f64::from)
            .filter(|&v| v.is_finite() && v > 0.0)
            .unwrap_or(1.0);
        let d = cornice.ingombro_mm() * MM_PT / unita;
        let s = cornice.spessore_mm.max(0.0) * MM_PT / unita;
        // Il rettangolo sta a metà dello spessore dentro il bordo della pagina:
        // il lato esterno della linea cade sul bordo del foglio (e quello
        // interno a `distanza` dal contenuto).
        let (rx0, ry0) = (x0 - d + s / 2.0, y0 - d + s / 2.0);
        let (larghezza, altezza) = (x1 - x0 + 2.0 * (d - s / 2.0), y1 - y0 + 2.0 * (d - s / 2.0));
        // Coordinate nello spazio utente non ruotato: con /Rotate il rettangolo
        // gira con la pagina.
        let linea =
            format!("\nQ\nq 0 G {s:.4} w {rx0:.4} {ry0:.4} {larghezza:.4} {altezza:.4} re S Q\n");
        // Conserva il ritaglio visibile prima di allargare i box, così la grafica
        // nascosta non invade lo spazio bianco. Ripristina poi lo stato iniziale:
        // matrice, clip e tratteggio lasciati dal contenuto non toccano la cornice.
        let ritaglio = format!("q {x0:.4} {y0:.4} {:.4} {:.4} re W n\n", x1 - x0, y1 - y0);
        impagina::aggiungi_intorno_al_contenuto(doc, id, ritaglio.into_bytes(), linea.into_bytes())?;
        let riquadro: Vec<Object> = [x0 - d, y0 - d, x1 + d, y1 + d]
            .iter()
            .map(|&v| Object::Real(v as f32))
            .collect();
        let pagina = doc.get_dictionary_mut(id)?;
        pagina.set("MediaBox", riquadro.clone());
        // Il CropBox è ereditabile: va riscritto, non solo tolto.
        pagina.set("CropBox", riquadro);
        for chiave in [&b"BleedBox"[..], b"TrimBox", b"ArtBox"] {
            pagina.remove(chiave);
        }
        cambiato = true;
    }
    Ok(cambiato)
}

/// Cosa serve per scalare una pagina.
struct ScalaPagina {
    /// Riquadro visibile `[x0, y0, x1, y1]` nello spazio utente.
    riquadro: [f64; 4],
    /// Fattori da applicare nello spazio utente: con `/Rotate` 90/270 la x
    /// visiva è la y dello spazio utente, quindi sono già scambiati.
    fattori: (f64, f64),
}

/// Tolleranza per "la scala non cambia la pagina". Larga abbastanza da
/// assorbire le misure in punti di un PDF: un A4 da 595,28 pt è 210,0014 mm, e
/// chiedere "A4" su un A4 non deve rimpicciolirlo di qualche milionesimo.
const TOLLERANZA_SCALA: f64 = 1e-4;

/// Fattori per una pagina, o `None` se non c'è niente da cambiare (pagina
/// senza box, `/Rotate` non multiplo di 90, oppure scala ≈ 100 %). È la sola
/// regola "questa pagina cambia?", usata sia per scalare sia per decidere se un
/// PDF cifrato si può lasciare com'è.
fn scala_pagina(doc: &Document, id: ObjectId, scala: &Scala) -> Option<ScalaPagina> {
    let ([x0, y0, x1, y1], ruota) = file::box_e_rotazione(doc, id)?;
    if ruota % 90 != 0 {
        return None;
    }
    let (larghezza, altezza) = ((x1 - x0) / MM_PT, (y1 - y0) / MM_PT);
    let (larghezza, altezza) = if ruota % 180 == 90 {
        (altezza, larghezza)
    } else {
        (larghezza, altezza)
    };
    let (sx_vis, sy_vis) = fattori(scala, larghezza, altezza);
    if (sx_vis - 1.0).abs() < TOLLERANZA_SCALA && (sy_vis - 1.0).abs() < TOLLERANZA_SCALA {
        return None;
    }
    let fattori = if ruota % 180 == 90 {
        (sy_vis, sx_vis)
    } else {
        (sx_vis, sy_vis)
    };
    Some(ScalaPagina {
        riquadro: [x0, y0, x1, y1],
        fattori,
    })
}

/// Scala una pagina in vettoriale. Le pagine senza box o con `/Rotate` non
/// multiplo di 90 restano come sono.
fn applica_scala_pagina(doc: &mut Document, id: ObjectId, scala: &Scala) -> Result<bool> {
    let Some(ScalaPagina {
        riquadro: [x0, y0, x1, y1],
        fattori: (sx, sy),
    }) = scala_pagina(doc, id, scala)
    else {
        return Ok(false);
    };

    // Il contenuto diventa un Form XObject con la matrice di scala: a differenza
    // di un `cm` sulla pagina, gradienti e motivi (pattern) seguono la scala.
    let contenuto = doc.get_page_content(id);
    let risorse = file::ereditato(doc, id, b"Resources")
        .cloned()
        .unwrap_or_else(|| Object::Dictionary(Dictionary::new()));
    let reali = |v: &[f64]| -> Vec<Object> { v.iter().map(|&n| Object::Real(n as f32)).collect() };
    let mut form = Dictionary::new();
    form.set("Type", "XObject");
    form.set("Subtype", "Form");
    form.set("BBox", reali(&[x0, y0, x1, y1]));
    form.set("Matrix", reali(&[sx, 0.0, 0.0, sy, -x0 * sx, -y0 * sy]));
    form.set("Resources", risorse);
    let mut form = Stream::new(form, contenuto);
    // Se non si comprime resta com'è: va bene lo stesso.
    let _ = form.compress();
    let form = doc.add_object(form);
    let chiamata = doc.add_object(Stream::new(Dictionary::new(), b"/IQScala Do\n".to_vec()));

    let riquadro = reali(&[0.0, 0.0, (x1 - x0) * sx, (y1 - y0) * sy]);
    let pagina = doc.get_dictionary_mut(id)?;
    pagina.set("Contents", chiamata);
    pagina.set(
        "Resources",
        dictionary! { "XObject" => dictionary! { "IQScala" => form } },
    );
    pagina.set("MediaBox", riquadro.clone());
    // Il CropBox è ereditabile: va riscritto, non solo tolto.
    pagina.set("CropBox", riquadro);
    for chiave in [&b"BleedBox"[..], b"TrimBox", b"ArtBox"] {
        pagina.remove(chiave);
    }
    trasforma_annotazioni(doc, id, (x0, y0), sx, sy);
    Ok(true)
}

/// Chiavi delle annotazioni che contengono coordinate in coppie x y (anche in
/// array annidati, come `/InkList`).
const CHIAVI_COORDINATE: [&[u8]; 6] = [b"Rect", b"QuadPoints", b"InkList", b"Vertices", b"L", b"CL"];

/// Porta le coordinate delle annotazioni nello spazio scalato: stessa matrice
/// del contenuto, cioè `(v - origine) * scala`. Il `/AP` si adatta da solo al
/// nuovo `/Rect`.
fn trasforma_annotazioni(
    doc: &mut Document,
    pagina: ObjectId,
    origine: (f64, f64),
    sx: f64,
    sy: f64,
) {
    let Some(valore) = doc
        .get_dictionary(pagina)
        .ok()
        .and_then(|d| d.get(b"Annots").ok())
        .cloned()
    else {
        return;
    };
    let (riferimento, mut lista) = match valore {
        Object::Array(lista) => (None, lista),
        Object::Reference(r) => match doc.get_object(r) {
            Ok(Object::Array(lista)) => (Some(r), lista.clone()),
            _ => return,
        },
        _ => return,
    };
    for voce in &mut lista {
        match voce {
            Object::Reference(id) => {
                let id = *id;
                let nuovi: Vec<(&[u8], Object)> = match doc.get_dictionary(id) {
                    Ok(d) => CHIAVI_COORDINATE
                        .iter()
                        .filter_map(|&chiave| {
                            let valore = d.get(chiave).ok()?;
                            Some((chiave, punti_scalati(doc, valore, origine, sx, sy)?))
                        })
                        .collect(),
                    Err(_) => continue,
                };
                if let Ok(d) = doc.get_dictionary_mut(id) {
                    for (chiave, nuovo) in nuovi {
                        d.set(chiave, nuovo);
                    }
                }
            }
            // Un'annotazione può anche stare inline nell'array (rara).
            Object::Dictionary(d) => {
                for chiave in CHIAVI_COORDINATE {
                    if let Ok(Object::Array(a)) = d.get(chiave)
                        && let Some(nuovo) = punti_da_array(a, origine, sx, sy)
                    {
                        d.set(chiave, Object::Array(nuovo));
                    }
                }
            }
            _ => {}
        }
    }
    match riferimento {
        Some(r) => {
            doc.objects.insert(r, Object::Array(lista));
        }
        None => {
            if let Ok(d) = doc.get_dictionary_mut(pagina) {
                d.set("Annots", Object::Array(lista));
            }
        }
    }
}

fn punti_scalati(
    doc: &Document,
    valore: &Object,
    origine: (f64, f64),
    sx: f64,
    sy: f64,
) -> Option<Object> {
    let array = match valore {
        Object::Array(a) => a,
        Object::Reference(_) => match doc.dereference(valore).ok()? {
            (_, Object::Array(a)) => a,
            _ => return None,
        },
        _ => return None,
    };
    punti_da_array(array, origine, sx, sy).map(Object::Array)
}

/// Coppie x y → `(v - origine) * scala`. Un elemento che è a sua volta un array
/// (le tratte di `/InkList`) riparte da x. `None` se c'è altro che numeri.
fn punti_da_array(array: &[Object], origine: (f64, f64), sx: f64, sy: f64) -> Option<Vec<Object>> {
    array
        .iter()
        .enumerate()
        .map(|(i, o)| match o {
            Object::Array(interno) => punti_da_array(interno, origine, sx, sy).map(Object::Array),
            _ => {
                let v = numero_o(o)?;
                let n = if i % 2 == 0 {
                    (v - origine.0) * sx
                } else {
                    (v - origine.1) * sy
                };
                Some(Object::Real(n as f32))
            }
        })
        .collect()
}

fn numero_o(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(f64::from(*r)),
        _ => None,
    }
}

/// Immagini: la scala non ricampiona, cambia solo i DPI (la T2600 li rispetta).
/// JPEG e TIFF partono nel loro formato, con i soli campi della risoluzione
/// riscritti (vedi `risoluzione.rs`); restano decodifica e TIFF RGB per PNG e
/// per l'allineamento sul rotolo, che devono ricomporre l'immagine.
fn prepara_immagine(
    percorso: &Path,
    formato: Formato,
    lav: &Lavorazione,
    imp: Option<&Impaginazione>,
) -> Result<Option<(FileTemporaneo, Formato)>> {
    let dpi = file::dpi_immagine(percorso, formato);
    let dimensioni = imagesize::size(percorso).context("dimensioni dell'immagine")?;
    let (larghezza, altezza) = (
        dimensioni.width as f64 * 25.4 / dpi.0,
        dimensioni.height as f64 * 25.4 / dpi.1,
    );
    let (sx, sy) = fattori(&lav.scala, larghezza, altezza);
    let scala = (sx - 1.0).abs() > 1e-9 || (sy - 1.0).abs() > 1e-9;
    let dpi_scalati = (dpi.0 / sx, dpi.1 / sy);
    let cornice = lav.cornice.as_ref();
    // La cornice fa parte del pezzo: entra nelle misure da mettere sul rotolo.
    let ingombro = cornice.map_or(0.0, |c| 2.0 * c.ingombro_mm());
    let impagina = imp.is_some_and(|imp| {
        imp.posiziona(larghezza * sx + ingombro, altezza * sy + ingombro)
            .is_some()
    });
    if !scala && !impagina && cornice.is_none() {
        return Ok(None);
    }
    // Solo scala su JPEG e TIFF: si correggono i DPI dichiarati, senza
    // decodificare (CMYK, grigi, 16 bit e profili ICC non si toccano). Se la
    // struttura non si sa riscrivere si ripiega sulla decodifica, come prima.
    // La cornice è una linea in più: lì il file va ricomposto comunque.
    if scala
        && !impagina
        && cornice.is_none()
        && matches!(formato, Formato::Jpeg | Formato::Tiff)
        && let Some(tmp) = risoluzione::con_dpi(percorso, formato, dpi_scalati)?
    {
        return Ok(Some((tmp, formato)));
    }

    let immagine = png::su_bianco(png::decodifica(percorso)?);
    if impagina {
        let imp = imp.expect("impagina implica un'impaginazione");
        let Ok(composto) = impagina::componi_raster(immagine, dpi_scalati, imp, cornice) else {
            return Ok(None);
        };
        return Ok(Some((composto.scrivi_tiff()?, Formato::Tiff)));
    }

    // Cornice senza allineamento: il pezzo esce della sua misura, con la linea
    // disegnata riga per riga mentre si scrive il TIFF.
    if let Some(cornice) = cornice {
        let composto = impagina::RasterComposto::con_cornice(immagine, dpi_scalati, cornice);
        return Ok(Some((composto.scrivi_tiff()?, Formato::Tiff)));
    }

    // Solo scala: stessi pixel, DPI nuovi.
    let riga = immagine.width() as usize * 3;
    let tiff = png::tiff_temporaneo(
        immagine.width(),
        immagine.height(),
        dpi_scalati,
        |y, out| {
            let inizio = y as usize * riga;
            out.copy_from_slice(&immagine.as_raw()[inizio..inizio + riga]);
        },
    )?;
    Ok(Some((tiff, Formato::Tiff)))
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::{Path, PathBuf};
    use tiff::decoder::Decoder;
    use tiff::decoder::ifd::Value;
    use tiff::tags::Tag;

    fn pt(mm: f64) -> f64 {
        mm * MM_PT
    }

    fn fattori_di(scala: Scala, l: f64, a: f64) -> (f64, f64) {
        let (sx, sy) = fattori(&scala, l, a);
        ((sx * 1e4).round() / 1e4, (sy * 1e4).round() / 1e4)
    }

    #[test]
    fn originale_non_cambia_niente() {
        assert_eq!(fattori(&Scala::Originale, 210.0, 297.0), (1.0, 1.0));
    }

    #[test]
    fn formato_adatta_tenendo_le_proporzioni() {
        // A1 verticale → A3: 297/594 = 0,5 e 420/841 ≈ 0,4994: vince il lato più stretto.
        let (sx, sy) = fattori_di(Scala::Formato(FormatoIso::A3), 594.0, 841.0);
        assert!((sx - 0.4994).abs() < 0.001 && sx == sy, "{sx} {sy}");
        // A4 verticale → A3: 297/210 = 420/297.
        let (sx, sy) = fattori_di(Scala::Formato(FormatoIso::A3), 210.0, 297.0);
        let atteso = (297.0_f64 / 210.0 * 1e4).round() / 1e4;
        assert!((sx - atteso).abs() < 0.001 && sx == sy, "{sx} {sy}");
    }

    #[test]
    fn formato_segue_l_orientamento_della_pagina() {
        // A4 orizzontale → A4 orizzontale: nessun cambio.
        assert_eq!(
            fattori(&Scala::Formato(FormatoIso::A4), 297.0, 210.0),
            (1.0, 1.0)
        );
        // A4 verticale → A4 resta verticale: nessun cambio.
        assert_eq!(
            fattori(&Scala::Formato(FormatoIso::A4), 210.0, 297.0),
            (1.0, 1.0)
        );
        // A3 orizzontale → A4: il formato diventa orizzontale (297 × 210).
        let (sx, _) = fattori_di(Scala::Formato(FormatoIso::A4), 420.0, 297.0);
        let atteso = (210.0_f64 / 297.0 * 1e4).round() / 1e4;
        assert!((sx - atteso).abs() < 0.001, "{sx}");
    }

    #[test]
    fn percentuale_e_misura() {
        assert_eq!(
            fattori(&Scala::Percentuale { x: 50.0, y: 25.0 }, 100.0, 200.0),
            (0.5, 0.25)
        );
        let dentro = Scala::Misura {
            larghezza_mm: 500.0,
            altezza_mm: 500.0,
            proporzioni: true,
        };
        assert_eq!(fattori(&dentro, 100.0, 200.0), (2.5, 2.5));
        let deformata = Scala::Misura {
            larghezza_mm: 500.0,
            altezza_mm: 500.0,
            proporzioni: false,
        };
        assert_eq!(fattori(&deformata, 100.0, 200.0), (5.0, 2.5));
    }

    #[test]
    fn fattori_limiti_e_pagine_degeneri() {
        assert_eq!(
            fattori(
                &Scala::Percentuale {
                    x: 0.001,
                    y: 5000.0
                },
                100.0,
                100.0
            ),
            (FATTORE_MIN, FATTORE_MAX)
        );
        assert_eq!(
            fattori(&Scala::Formato(FormatoIso::A4), 0.0, 100.0),
            (1.0, 1.0)
        );
        assert_eq!(
            fattori(&Scala::Formato(FormatoIso::A4), f64::NAN, 100.0),
            (1.0, 1.0)
        );
    }

    #[test]
    fn etichette_della_scala() {
        assert_eq!(Scala::Originale.etichetta(), "Originale");
        assert_eq!(Scala::Formato(FormatoIso::A2).etichetta(), "A2");
        assert_eq!(Scala::Percentuale { x: 50.0, y: 50.0 }.etichetta(), "50 %");
        assert_eq!(
            Scala::Percentuale { x: 50.0, y: 70.0 }.etichetta(),
            "50 × 70 %"
        );
        assert_eq!(
            Scala::Misura {
                larghezza_mm: 500.0,
                altezza_mm: 700.0,
                proporzioni: true
            }
            .etichetta(),
            "500 × 700 mm"
        );
        assert_eq!(Scala::Formato(FormatoIso::A3).didascalia(), "→ A3");
        assert!(Scala::Originale.didascalia().is_empty());
    }

    #[test]
    fn parser_della_riga_di_comando() {
        assert_eq!(Scala::da_testo("A3"), Some(Scala::Formato(FormatoIso::A3)));
        assert_eq!(Scala::da_testo("a0"), Some(Scala::Formato(FormatoIso::A0)));
        assert_eq!(
            Scala::da_testo("50%"),
            Some(Scala::Percentuale { x: 50.0, y: 50.0 })
        );
        assert_eq!(
            Scala::da_testo("500x700"),
            Some(Scala::Misura {
                larghezza_mm: 500.0,
                altezza_mm: 700.0,
                proporzioni: true
            })
        );
        assert_eq!(
            Scala::da_testo("500×700!"),
            Some(Scala::Misura {
                larghezza_mm: 500.0,
                altezza_mm: 700.0,
                proporzioni: false
            })
        );
        assert_eq!(Scala::da_testo("originale"), Some(Scala::Originale));
        for brutto in ["", "A5", "0%", "1001%", "500", "500x0", "-1x2", "ciao"] {
            assert_eq!(Scala::da_testo(brutto), None, "{brutto:?}");
        }
    }

    /// PDF di una pagina A4 con un'annotazione e un rettangolo di contenuto.
    fn pdf_a4_con_annotazione() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pagine_id = doc.new_object_id();
        let contenuto = doc.add_object(Stream::new(
            Dictionary::new(),
            b"0 0 0 rg 10 20 100 50 re f".to_vec(),
        ));
        let annotazione = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Square",
            "Rect" => vec![10.into(), 20.into(), 110.into(), 70.into()],
            "QuadPoints" => vec![
                10.into(), 70.into(), 110.into(), 70.into(),
                10.into(), 20.into(), 110.into(), 20.into(),
            ],
        });
        let media_box = Object::Array(
            [0.0, 0.0, pt(210.0), pt(297.0)]
                .iter()
                .map(|&v| Object::Real(v as f32))
                .collect(),
        );
        let pagina = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pagine_id,
            "Contents" => contenuto,
            "MediaBox" => media_box,
            "Annots" => vec![Object::Reference(annotazione)],
        });
        doc.objects.insert(
            pagine_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![pagina.into()],
                "Count" => 1,
            }),
        );
        let catalogo = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pagine_id });
        doc.trailer.set("Root", catalogo);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn pagina(dati: &[u8]) -> (Document, ObjectId) {
        let doc = Document::load_mem(dati).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        (doc, id)
    }

    /// Il Form XObject in cui la scala avvolge il contenuto della pagina.
    fn form_di_scala(doc: &Document, id: ObjectId) -> Stream {
        let testo = String::from_utf8_lossy(&doc.get_page_content(id)).into_owned();
        assert!(testo.contains("/IQScala Do"), "{testo}");
        let pagina = doc.get_dictionary(id).unwrap();
        let risorse = pagina.get(b"Resources").unwrap().as_dict().unwrap();
        let form = risorse
            .get(b"XObject")
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"IQScala")
            .unwrap()
            .as_reference()
            .unwrap();
        doc.get_object(form).unwrap().as_stream().unwrap().clone()
    }

    fn matrice_di_scala(doc: &Document, id: ObjectId) -> Vec<f64> {
        form_di_scala(doc, id)
            .dict
            .get(b"Matrix")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| numero_o(o).unwrap())
            .collect()
    }

    #[test]
    fn pdf_a4_verso_a5_scala_box_contenuto_e_annotazioni() {
        let dati = pdf_a4_con_annotazione();
        let composto = componi_pdf(
            &dati,
            &Lavorazione {
                scala: Scala::Misura {
                    larghezza_mm: 148.0,
                    altezza_mm: 210.0,
                    proporzioni: true,
                },
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .unwrap();
        let (doc, id) = pagina(&composto.dati);
        let ([x0, y0, x1, y1], ruota) = file::box_e_rotazione(&doc, id).unwrap();
        assert_eq!((x0, y0), (0.0, 0.0));
        assert_eq!(ruota, 0);
        let s = (pt(148.0) / pt(210.0)).min(pt(210.0) / pt(297.0));
        assert!((x1 - pt(210.0) * s).abs() < 0.01, "larghezza {}", x1);
        assert!((y1 - pt(297.0) * s).abs() < 0.01, "altezza {}", y1);
        // Il contenuto sta in un Form XObject con la matrice di scala.
        let m = matrice_di_scala(&doc, id);
        assert!((m[0] - s).abs() < 1e-5 && (m[3] - s).abs() < 1e-5, "{m:?}");
        // L'annotazione segue la stessa matrice.
        let annot = doc
            .get_dictionary(*doc.get_pages().values().next().unwrap())
            .unwrap()
            .get(b"Annots")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .as_reference()
            .unwrap();
        let annot = doc.get_dictionary(annot).unwrap();
        let rect: Vec<f64> = annot
            .get(b"Rect")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| numero_o(o).unwrap())
            .collect();
        assert!((rect[0] - 10.0 * s).abs() < 0.01, "{rect:?}");
        assert!((rect[1] - 20.0 * s).abs() < 0.01, "{rect:?}");
        assert!((rect[2] - 110.0 * s).abs() < 0.01, "{rect:?}");
        assert!((rect[3] - 70.0 * s).abs() < 0.01, "{rect:?}");        let quad = annot.get(b"QuadPoints").unwrap().as_array().unwrap();
        assert!((numero_o(&quad[1]).unwrap() - 70.0 * s).abs() < 0.01, "{quad:?}");
    }

    #[test]
    fn annotazioni_con_origine_spostata_seguono_il_contenuto() {
        // Pagina con MediaBox che parte da (50, 30); l'annotazione sta sul
        // rettangolo di contenuto (50..150, 30..80).
        let base = impagina::test::pdf(100.0, 50.0, (50.0, 30.0), 0);
        let mut doc = Document::load_mem(&base).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        let interi = |v: &[i64]| -> Vec<Object> { v.iter().map(|&n| n.into()).collect() };
        let annotazione = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Ink",
            "Rect" => interi(&[50, 30, 150, 80]),
            "InkList" => vec![
                Object::Array(interi(&[50, 30, 150, 80])),
                Object::Array(interi(&[100, 40])),
            ],
        });
        doc.get_dictionary_mut(id)
            .unwrap()
            .set("Annots", vec![Object::Reference(annotazione)]);
        let mut dati = Vec::new();
        doc.save_to(&mut dati).unwrap();

        let composto = componi_pdf(
            &dati,
            &Lavorazione {
                scala: Scala::Percentuale {
                    x: 200.0,
                    y: 200.0,
                },
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .unwrap();
        let (doc, id) = pagina(&composto.dati);
        let annot = doc.get_dictionary(id).unwrap().get(b"Annots").unwrap().as_array().unwrap()[0]
            .as_reference()
            .unwrap();
        let annot = doc.get_dictionary(annot).unwrap();
        let numeri = |o: &Object| -> Vec<f64> {
            o.as_array().unwrap().iter().map(|o| numero_o(o).unwrap()).collect()
        };
        // (v - origine) * 2: il rettangolo di contenuto diventa 0..200 × 0..100.
        assert_eq!(numeri(annot.get(b"Rect").unwrap()), [0.0, 0.0, 200.0, 100.0]);
        let ink = annot.get(b"InkList").unwrap().as_array().unwrap();
        assert_eq!(numeri(&ink[0]), [0.0, 0.0, 200.0, 100.0]);
        assert_eq!(numeri(&ink[1]), [100.0, 20.0]);
    }

    #[test]
    fn pdf_con_origine_spostata_riporta_il_contenuto_a_zero() {
        let (l, a) = (210.0 * MM_PT, 297.0 * MM_PT);
        let dati = impagina::test::pdf(l, a, (50.0, 30.0), 0);
        let composto = componi_pdf(
            &dati,
            &Lavorazione {
                scala: Scala::Percentuale {
                    x: 50.0,
                    y: 50.0,
                },
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .unwrap();
        let (doc, id) = pagina(&composto.dati);
        let ([x0, y0, _, _], _) = file::box_e_rotazione(&doc, id).unwrap();
        assert!(x0.abs() < 0.01 && y0.abs() < 0.01, "{x0} {y0}");
        let m = matrice_di_scala(&doc, id);
        assert!((m[4] + 25.0).abs() < 1e-4 && (m[5] + 15.0).abs() < 1e-4, "{m:?}");
    }

    #[test]
    fn pdf_con_rotate_90_scambia_i_fattori() {
        // Pagina 210 × 297 con /Rotate 90: appare 297 × 210. Al 50% × 25% i
        // fattori nello spazio utente sono scambiati.
        let mut doc = Document::load_mem(&pdf_a4_con_annotazione()).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        doc.get_dictionary_mut(id).unwrap().set("Rotate", 90);
        let mut dati = Vec::new();
        doc.save_to(&mut dati).unwrap();

        let composto = componi_pdf(
            &dati,
            &Lavorazione {
                scala: Scala::Percentuale { x: 50.0, y: 25.0 },
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .unwrap();
        let (doc, id) = pagina(&composto.dati);
        let ([_, _, x1, y1], ruota) = file::box_e_rotazione(&doc, id).unwrap();
        assert_eq!(ruota, 90);
        // Larghezza utente = 210 × 0,25, altezza = 297 × 0,5.
        assert!((x1 - pt(210.0) * 0.25).abs() < 0.01, "{x1}");
        assert!((y1 - pt(297.0) * 0.5).abs() < 0.01, "{y1}");
        let m = matrice_di_scala(&doc, id);
        assert!((m[0] - 0.25).abs() < 1e-5 && (m[3] - 0.5).abs() < 1e-5, "{m:?}");
    }

    #[test]
    fn le_risorse_della_pagina_seguono_il_contenuto_nel_form() {
        // Gradienti e motivi vivono nelle risorse: devono stare nel form, che li scala.
        let mut doc = Document::load_mem(&pdf_a4_con_annotazione()).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        doc.get_dictionary_mut(id)
            .unwrap()
            .set("Resources", dictionary! { "Pattern" => dictionary! {} });
        let mut dati = Vec::new();
        doc.save_to(&mut dati).unwrap();
        let composto = componi_pdf(
            &dati,
            &Lavorazione { scala: Scala::Percentuale { x: 50.0, y: 50.0 }, ..Default::default() },
            None,
        )
        .unwrap()
        .unwrap();
        let (doc, id) = pagina(&composto.dati);
        let form = form_di_scala(&doc, id);
        assert!(form.dict.get(b"Resources").unwrap().as_dict().unwrap().has(b"Pattern"));
    }

    #[test]
    fn il_cropbox_decide_la_pagina_visibile() {
        // MediaBox con abbondanza, CropBox A4: la scala parte dall'A4 e il
        // contenuto fuori dal CropBox resta fuori pagina.
        let mut doc = Document::load_mem(&pdf_a4_con_annotazione()).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        let (l, a) = (pt(210.0), pt(297.0));
        let dizionario = doc.get_dictionary_mut(id).unwrap();
        dizionario.set("MediaBox", vec![(-pt(3.0)).into(), (-pt(3.0)).into(), (l + pt(3.0)).into(), (a + pt(3.0)).into()]);
        dizionario.set("CropBox", vec![0.into(), 0.into(), l.into(), a.into()]);
        let mut dati = Vec::new();
        doc.save_to(&mut dati).unwrap();
        let composto = componi_pdf(
            &dati,
            &Lavorazione { scala: Scala::Percentuale { x: 50.0, y: 50.0 }, ..Default::default() },
            None,
        )
        .unwrap()
        .unwrap();
        let (doc, id) = pagina(&composto.dati);
        let ([_, _, x1, y1], _) = file::box_e_rotazione(&doc, id).unwrap();
        assert!((x1 - l * 0.5).abs() < 0.01 && (y1 - a * 0.5).abs() < 0.01, "{x1} {y1}");
    }

    #[test]
    fn cento_percento_e_come_l_originale() {
        let lav = Lavorazione { scala: Scala::Percentuale { x: 100.0, y: 100.0 }, ..Default::default() };
        assert!(lav.nulla());
        // Un PDF illeggibile non fallisce se la scala non cambia niente.
        assert!(componi_pdf(b"non e un pdf", &lav, None).unwrap().is_none());
    }

    #[test]
    fn limiti_della_scala() {
        let grande = Scala::Misura { larghezza_mm: 5000.0, altezza_mm: 7000.0, proporzioni: true };
        assert!(fuori_limiti(&grande, 210.0, 297.0));
        assert!(!fuori_limiti(&Scala::Formato(FormatoIso::A3), 210.0, 297.0));
        assert!(!fuori_limiti(&Scala::Percentuale { x: 1000.0, y: 1000.0 }, 210.0, 297.0));
        let ((min_l, _), (max_l, max_a)) = limiti_mm(210.0, 297.0);
        assert!((min_l - 2.1).abs() < 1e-9 && (max_l - 2100.0).abs() < 1e-9 && (max_a - 2970.0).abs() < 1e-9);
    }

    #[test]
    fn pdf_pagina_senza_box_resta_come_e() {
        // Un PDF senza MediaBox non deve rompere la scala.
        let mut doc = Document::with_version("1.5");
        let pagine_id = doc.new_object_id();
        let pagina = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pagine_id, "Contents" => Stream::new(Dictionary::new(), vec![]),
        });
        doc.objects.insert(
            pagine_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![pagina.into()], "Count" => 1,
            }),
        );
        let catalogo = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pagine_id });
        doc.trailer.set("Root", catalogo);
        let mut dati = Vec::new();
        doc.save_to(&mut dati).unwrap();
        assert!(
            componi_pdf(
                &dati,
                &Lavorazione {
                    scala: Scala::Formato(FormatoIso::A4),
                    ..Default::default()
                },
                None
            )
            .unwrap()
            .is_none()
        );
    }

    fn cartella(nome: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("iqclock-lavorazione-{}-{nome}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// PNG 100 × 50 px dichiarato a 100 dpi: 25,4 × 12,7 mm.
    fn png_100dpi(dir: &Path) -> PathBuf {
        let p = dir.join("foto.png");
        image::RgbImage::from_pixel(100, 50, image::Rgb([20, 40, 60]))
            .save(&p)
            .unwrap();
        // 100 dpi = 3937 pixel per metro.
        std::fs::write(&p, png::test::con_phys(&std::fs::read(&p).unwrap(), 3937)).unwrap();
        p
    }

    fn dpi_tiff(percorso: &Path) -> (f64, f64) {
        let mut d = Decoder::new(std::fs::File::open(percorso).unwrap()).unwrap();
        let razionale = |v: Value| match v {
            Value::Rational(n, c) => n as f64 / c as f64,
            v => panic!("risoluzione non razionale: {v:?}"),
        };
        (
            razionale(d.get_tag(Tag::XResolution).unwrap()),
            razionale(d.get_tag(Tag::YResolution).unwrap()),
        )
    }

    #[test]
    fn immagine_al_200_percento_dimezza_i_dpi_e_tiene_i_pixel() {
        let dir = cartella("dpi");
        let p = png_100dpi(&dir);
        let lav = Lavorazione {
            scala: Scala::Percentuale { x: 200.0, y: 200.0 },
            ..Default::default()
        };
        let (tiff, formato) = prepara(&p, Formato::Png, &lav, None).unwrap().unwrap();
        assert_eq!(formato, Formato::Tiff);
        let letta = image::open(tiff.percorso()).unwrap().to_rgb8();
        assert_eq!(letta.dimensions(), (100, 50), "i pixel non si toccano");
        assert_eq!(letta.get_pixel(10, 10).0, [20, 40, 60]);
        let (x, y) = dpi_tiff(tiff.percorso());
        assert!((x - 50.0).abs() < 0.1 && (y - 50.0).abs() < 0.1, "{x} {y}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn immagine_senza_scala_e_senza_allineamento_parte_com_e() {
        let dir = cartella("originale");
        let p = png_100dpi(&dir);
        assert!(
            prepara(&p, Formato::Png, &Lavorazione::default(), None)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scala_e_allineamento_centrato_usano_le_misure_scalate() {
        // 100 × 50 px a 100 dpi, al 200%: 50,8 × 25,4 mm a 50 dpi. Rotolo da
        // 100 mm e margine 5: pagina da 90 mm = 177 px, contenuto centrato.
        let dir = cartella("centro");
        let p = png_100dpi(&dir);
        let lav = Lavorazione {
            scala: Scala::Percentuale { x: 200.0, y: 200.0 },
            ..Default::default()
        };
        let imp = Impaginazione {
            allineamento: impagina::Allineamento::Centro,
            rotolo_mm: 100.0,
            margine_mm: 5.0,
        };
        let (tiff, _) = prepara(&p, Formato::Png, &lav, Some(&imp))
            .unwrap()
            .unwrap();
        let letta = image::open(tiff.percorso()).unwrap().to_rgb8();
        assert_eq!(letta.dimensions(), (177, 50));
        let x = ((90.0_f64 - 50.8) / 2.0 / 25.4 * 50.0).round() as u32;
        assert_eq!(x, 39);
        assert_eq!(letta.get_pixel(x - 1, 10).0, [255, 255, 255]);
        assert_eq!(letta.get_pixel(x, 10).0, [20, 40, 60]);
        assert_eq!(letta.get_pixel(x + 99, 10).0, [20, 40, 60]);
        let (dx, _) = dpi_tiff(tiff.percorso());
        assert!((dx - 50.0).abs() < 0.1, "{dx}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn la_pagina_cambia_solo_se_la_scala_la_tocca() {
        let doc = Document::load_mem(&pdf_a4_con_annotazione()).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        assert!(scala_pagina(&doc, id, &Scala::Formato(FormatoIso::A4)).is_none());
        assert!(scala_pagina(&doc, id, &Scala::Percentuale { x: 100.0, y: 100.0 }).is_none());
        assert!(scala_pagina(&doc, id, &Scala::Formato(FormatoIso::A3)).is_some());
        assert!(scala_pagina(&doc, id, &Scala::Percentuale { x: 50.0, y: 50.0 }).is_some());

        // Le misure "vere" di un A4 in punti (595,28 × 841,89) non sono un
        // cambio: la tolleranza assorbe i millesimi di millimetro.
        let dati = impagina::test::pdf(595.28, 841.89, (0.0, 0.0), 0);
        let doc = Document::load_mem(&dati).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        assert!(scala_pagina(&doc, id, &Scala::Formato(FormatoIso::A4)).is_none());
    }

    #[test]
    fn jpeg_al_200_percento_resta_jpeg_con_meta_dpi() {
        let dir = cartella("jpeg-scala");
        let p = dir.join("foto.jpg");
        image::RgbImage::from_fn(40, 30, |x, y| image::Rgb([x as u8, y as u8, 9]))
            .save_with_format(&p, image::ImageFormat::Jpeg)
            .unwrap();
        let lav = Lavorazione {
            scala: Scala::Percentuale { x: 200.0, y: 200.0 },
            ..Default::default()
        };
        let (tmp, formato) = prepara(&p, Formato::Jpeg, &lav, None).unwrap().unwrap();
        // Non passa più da un TIFF RGB: resta un JPEG con i DPI dimezzati.
        assert_eq!(formato, Formato::Jpeg);
        assert_eq!(
            file::dpi_immagine(tmp.percorso(), Formato::Jpeg),
            (150.0, 150.0)
        );
        let letta = image::open(tmp.percorso()).unwrap().to_rgb8();
        assert_eq!(letta.dimensions(), (40, 30), "i pixel non si toccano");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tiff_cmyk_con_sola_scala_resta_cmyk() {
        use tiff::decoder::{DecodingResult, Decoder as Lettore};
        use tiff::encoder::{Rational, TiffEncoder, colortype};
        let dir = cartella("cmyk");
        let p = dir.join("cmyk.tif");
        let dati: Vec<u8> = (0..30 * 20 * 4).map(|i| (i % 251) as u8).collect();
        {
            let mut enc = TiffEncoder::new(std::fs::File::create(&p).unwrap()).unwrap();
            let mut img = enc.new_image::<colortype::CMYK8>(30, 20).unwrap();
            img.resolution_unit(tiff::tags::ResolutionUnit::Inch);
            img.x_resolution(Rational { n: 300, d: 1 });
            img.y_resolution(Rational { n: 300, d: 1 });
            img.write_data(&dati).unwrap();
        }
        let lav = Lavorazione {
            scala: Scala::Percentuale { x: 200.0, y: 200.0 },
            ..Default::default()
        };
        let (tmp, formato) = prepara(&p, Formato::Tiff, &lav, None).unwrap().unwrap();
        assert_eq!(formato, Formato::Tiff);
        let mut lettore = Lettore::new(std::fs::File::open(tmp.percorso()).unwrap()).unwrap();
        // Se fosse passato dalla decodifica sarebbe un TIFF RGB 8 bit.
        assert_eq!(lettore.colortype().unwrap(), tiff::ColorType::CMYK(8));
        assert_eq!(lettore.dimensions().unwrap(), (30, 20));
        let pixel = match lettore.read_image().unwrap() {
            DecodingResult::U8(v) => v,
            _ => panic!("pixel inattesi"),
        };
        assert_eq!(pixel, dati);
        assert_eq!(
            file::dpi_immagine(tmp.percorso(), Formato::Tiff),
            (150.0, 150.0)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn composto_nessuna_modifica_resta_none() {
        assert!(
            componi_pdf(&pdf_a4_con_annotazione(), &Lavorazione::default(), None)
                .unwrap()
                .is_none()
        );
    }

    /// PDF cifrato con lopdf (password vuota).
    fn cifra(dati: &[u8]) -> Vec<u8> {
        use lopdf::encryption::{EncryptionState, EncryptionVersion, Permissions};
        // La cifratura vuole un /ID nel trailer; due copie con lo stesso ID,
        // una per lo stato di cifratura e una da cifrare.
        let con_id = || {
            let mut doc = Document::load_mem(dati).unwrap();
            let id = |n: u8| Object::String(vec![n; 16], lopdf::StringFormat::Hexadecimal);
            doc.trailer.set("ID", vec![id(1), id(2)]);
            doc
        };
        // Lo stato di cifratura si costruisce su un documento di riferimento:
        // lo stesso di cui poi si cifra una copia.
        let riferimento = con_id();
        let stato = EncryptionState::try_from(EncryptionVersion::V2 {
            document: &riferimento,
            // Con la password vuota lopdf lo decifra da solo al caricamento:
            // ne serve una vera per restare cifrato.
            owner_password: "segreta",
            user_password: "segreta",
            key_length: 128,
            permissions: Permissions::all(),
        })
        .unwrap();
        let mut doc = con_id();
        doc.encrypt(&stato).unwrap();
        // Il dizionario /Encrypt nel trailer: senza, il file riletto sembra
        // normale (con gli oggetti però già cifrati).
        let cifratura = doc.add_object(Object::Dictionary(stato.encode().unwrap()));
        doc.trailer.set("Encrypt", Object::Reference(cifratura));
        // Senza object stream, come tanti PDF veri: il dizionario delle pagine
        // resta leggibile anche cifrato (la cifratura tocca stringhe e stream).
        let mut out = Vec::new();
        doc.save_with_options(
            &mut out,
            lopdf::SaveOptions {
                use_object_streams: false,
                use_xref_streams: false,
                ..Default::default()
            },
        )
        .unwrap();
        out
    }

    #[test]
    fn pdf_cifrato_senza_password_resta_da_originale() {
        // lopdf, senza la password, si ferma prima di leggere il resto del
        // documento: le pagine non ci sono e la scala non si può controllare.
        // Resta il rifiuto (fino a quando lopdf non leggerà i box anche
        // cifrati: lì il ramo in `componi_pdf` scala solo ciò che cambia).
        let cifrato = cifra(&impagina::test::pdf(pt(210.0), pt(297.0), (0.0, 0.0), 0));
        let lav = Lavorazione {
            scala: Scala::Formato(FormatoIso::A4),
            ..Default::default()
        };
        let Err(err) = componi_pdf(&cifrato, &lav, None) else {
            panic!("un PDF cifrato senza password non può essere composto");
        };
        assert!(err.to_string().contains("pagine non leggibili"), "{err}");
    }

    /// Box della pagina composta, in mm.
    fn box_mm(doc: &Document, id: ObjectId) -> (f64, f64, f64, f64) {
        let ([x0, y0, x1, y1], _) = file::box_e_rotazione(doc, id).unwrap();
        (x0 / MM_PT, y0 / MM_PT, x1 / MM_PT, y1 / MM_PT)
    }

    #[test]
    fn la_sola_cornice_non_e_nulla() {
        let lav = Lavorazione { cornice: Some(Cornice::default()), ..Default::default() };
        assert!(!lav.nulla());
        // La cornice tocca le pagine: un PDF illeggibile non può partire com'è.
        assert!(componi_pdf(b"non e un pdf", &lav, None).is_err());
    }

    #[test]
    fn cornice_sul_pdf_allarga_il_box_e_disegna_la_linea() {
        // A4 (210 × 297 mm) con cornice 3 + 0,25: il foglio cresce di 3,25 per
        // lato, cioè 216,5 × 303,5 mm.
        let composto = componi_pdf(
            &pdf_a4_con_annotazione(),
            &Lavorazione { cornice: Some(Cornice::default()), ..Default::default() },
            None,
        )
        .unwrap()
        .unwrap();
        let (doc, id) = pagina(&composto.dati);
        let (x0, y0, x1, y1) = box_mm(&doc, id);
        assert!((x1 - x0 - 216.5).abs() < 0.01, "larghezza {x1} {x0}");
        assert!((y1 - y0 - 303.5).abs() < 0.01, "altezza {y1} {y0}");
        assert!((x0 + 3.25).abs() < 0.01 && (y0 + 3.25).abs() < 0.01, "{x0} {y0}");
        let testo = String::from_utf8_lossy(&doc.get_page_content(id)).into_owned();
        // Il contenuto originale è intatto e la linea arriva dopo.
        assert!(testo.contains("re f"), "contenuto originale: {testo}");
        assert!(testo.contains("0 G"), "linea nera: {testo}");
        assert!(testo.trim_end().ends_with("re S Q"), "linea in coda: {testo}");
    }

    /// Verifica misure fisiche costanti su pagine con UserUnit assente, 1, 10 e 0,5.
    #[test]
    fn cornice_rispetta_le_unita_fisiche_di_ogni_pagina() {
        let dati = impagina::test::pdf(200.0, 300.0, (10.0, 20.0), 0);
        let (mut doc, primo) = pagina(&dati);
        let modello = doc.get_dictionary(primo).unwrap().clone();
        let padre = modello.get(b"Parent").unwrap().as_reference().unwrap();
        let mut pagine = vec![(primo, 1.0)]; // /UserUnit assente: vale 1.
        for unita in [1.0_f32, 10.0, 0.5] {
            let mut pagina = modello.clone();
            let riferimento = doc.add_object(Object::Real(unita));
            pagina.set("UserUnit", riferimento);
            pagine.push((doc.add_object(pagina), f64::from(unita)));
        }
        let kids: Vec<Object> = pagine.iter().map(|&(id, _)| id.into()).collect();
        let padre = doc.get_dictionary_mut(padre).unwrap();
        padre.set("Kids", kids);
        padre.set("Count", pagine.len() as i64);

        applica_cornice(&mut doc, &Cornice::default()).unwrap();
        for (id, unita) in pagine {
            let ([x0, y0, x1, y1], _) = file::box_e_rotazione(&doc, id).unwrap();
            // L'ingombro fisico deve restare 3,25 mm su tutti e quattro i lati.
            for delta in [10.0 - x0, 20.0 - y0, x1 - 210.0, y1 - 320.0] {
                assert!((delta * unita / MM_PT - 3.25).abs() < 0.001);
            }
            let contenuto = lopdf::content::Content::decode(&doc.get_page_content(id)).unwrap();
            let spessore = contenuto.operations.iter().find(|op| op.operator == "w").unwrap();
            let spessore = f64::from(spessore.operands[0].as_float().unwrap());
            assert!((spessore * unita / MM_PT - 0.25).abs() < 0.001);
            let linea = contenuto.operations.iter().rev().find(|op| op.operator == "re").unwrap();
            let x_linea = f64::from(linea.operands[0].as_float().unwrap());
            let distanza = (10.0 - x_linea - spessore / 2.0) * unita / MM_PT;
            assert!((distanza - 3.0).abs() < 0.001);
        }
    }

    #[test]
    fn cornice_isola_lo_stato_grafico_del_contenuto() {
        let dati = impagina::test::pdf(200.0, 300.0, (10.0, 20.0), 90);
        let (mut doc, id) = pagina(&dati);
        // Stato finale valido, ma diverso da quello iniziale: trasformazione,
        // clip e tratteggio. Distribuito su più stream senza q/Q esterni.
        let originali = [
            b"2 0 0 2 50 60 cm\n".to_vec(),
            b"0 0 10 10 re W n [3 2] 1 d".to_vec(),
        ];
        let contenuti: Vec<Object> = originali
            .iter()
            .map(|contenuto| {
                doc.add_object(Stream::new(Dictionary::new(), contenuto.clone()))
                    .into()
            })
            .collect();
        doc.get_dictionary_mut(id).unwrap().set("Contents", contenuti);
        applica_cornice(&mut doc, &Cornice::default()).unwrap();
        let contenuto = lopdf::content::Content::decode(&doc.get_page_content(id)).unwrap();
        let mut profondita = 0;
        let mut modifiche = 0;
        for op in contenuto.operations {
            match op.operator.as_str() {
                "q" => profondita += 1,
                "Q" => profondita -= 1,
                "cm" | "d" => {
                    // Le modifiche originali devono essere protette da un save.
                    assert_eq!(profondita, 1);
                    modifiche += 1;
                }
                "S" => {
                    // Il save della cornice deve partire dallo stato iniziale,
                    // dopo che il restore ha chiuso quello del contenuto.
                    assert_eq!(profondita, 1);
                    assert_eq!(modifiche, 2);
                }
                _ => {}
            }
            assert!(profondita >= 0);
        }
        assert_eq!(profondita, 0);
    }

    #[test]
    fn cornice_conserva_il_clip_visibile_prima_di_allargare_i_box() {
        for crop in [None, Some(vec![20.into(), 30.into(), 190.into(), 280.into()])] {
            let dati = impagina::test::pdf(200.0, 300.0, (10.0, 20.0), 0);
            let (mut doc, id) = pagina(&dati);
            if let Some(crop) = crop {
                doc.get_dictionary_mut(id).unwrap().set("CropBox", crop);
            }
            let ([x0, y0, x1, y1], _) = file::box_e_rotazione(&doc, id).unwrap();
            // Grafica che copre anche l'area esterna al MediaBox/CropBox.
            let originale = doc.add_object(Stream::new(
                Dictionary::new(),
                b"-100 -100 1000 1000 re f".to_vec(),
            ));
            doc.get_dictionary_mut(id).unwrap().set("Contents", originale);
            applica_cornice(&mut doc, &Cornice::default()).unwrap();
            let contenuto = lopdf::content::Content::decode(&doc.get_page_content(id)).unwrap();
            let ops = &contenuto.operations;
            // Il clip usa il vecchio box nello spazio pagina, prima della grafica.
            assert_eq!(ops[0].operator, "q");
            assert_eq!(ops[1].operator, "re");
            let rettangolo: Vec<f64> = ops[1].operands.iter()
                .map(|v| f64::from(v.as_float().unwrap()))
                .collect();
            assert_eq!(rettangolo, [x0, y0, x1 - x0, y1 - y0]);
            assert_eq!(ops[2].operator, "W");
            assert_eq!(ops[3].operator, "n");
            assert_eq!(ops[4].operator, "re");
            assert_eq!(ops[5].operator, "f");
            // La cornice è fuori dal clip originale.
            assert_eq!(ops[6].operator, "Q");
            assert_eq!(ops[7].operator, "q");
            assert!(ops[8..].iter().any(|op| op.operator == "S"));
        }
    }

    #[test]
    fn cornice_e_allineamento_contano_la_cornice_nel_pezzo() {
        // A5 orizzontale (210 × 148) centrato su rotolo da 914: il pezzo è
        // 216,5 × 154,5, quindi la pagina si allarga di quel che resta.
        let (l, a) = (210.0 * MM_PT, 148.0 * MM_PT);
        let dati = impagina::test::pdf(l, a, (0.0, 0.0), 0);
        let imp = Impaginazione {
            allineamento: impagina::Allineamento::Centro,
            rotolo_mm: 914.0,
            margine_mm: 5.0,
        };
        let composto = componi_pdf(
            &dati,
            &Lavorazione { cornice: Some(Cornice::default()), ..Default::default() },
            Some(&imp),
        )
        .unwrap()
        .unwrap();
        assert!(composto.posizioni[0].is_some());
        let (doc, id) = pagina(&composto.dati);
        let (x0, _, x1, _) = box_mm(&doc, id);
        assert!((x1 - x0 - 904.0).abs() < 0.01, "pagina {x0} {x1}");
        // Il pezzo (cornice compresa) resta centrato: la linea sta a 3,25 mm dai
        // bordi del pezzo, non da quelli della pagina allargata.
        let dx = (904.0 - 216.5) / 2.0;
        assert!((x0 + dx + 3.25).abs() < 0.05, "bordo del pezzo a {x0}");
        let testo = String::from_utf8_lossy(&doc.get_page_content(id)).into_owned();
        assert!(testo.contains("re S"), "{testo}");
        assert!(testo.contains("re f"), "{testo}");
    }

    #[test]
    fn cornice_sull_immagine_allarga_i_pixel_e_disegna_il_bordo() {
        let dir = cartella("cornice");
        let p = png_100dpi(&dir);
        let lav = Lavorazione { cornice: Some(Cornice::default()), ..Default::default() };
        let (tiff, formato) = prepara(&p, Formato::Png, &lav, None).unwrap().unwrap();
        assert_eq!(formato, Formato::Tiff);
        let letta = image::open(tiff.percorso()).unwrap().to_rgb8();
        // 3,25 mm a 100 dpi, 13 px per lato.
        assert_eq!(letta.dimensions(), (100 + 2 * 13, 50 + 2 * 13));
        assert_eq!(letta.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(letta.get_pixel(100 + 2 * 13 - 1, 50 + 2 * 13 - 1).0, [0, 0, 0]);
        // Contenuto al suo posto, con la distanza bianca attorno.
        assert_eq!(letta.get_pixel(13 + 10, 13 + 10).0, [20, 40, 60]);
        assert_eq!(letta.get_pixel(12, 30).0, [255, 255, 255]);
        assert_eq!(letta.get_pixel(60, 12).0, [255, 255, 255]);
        let (dx, dy) = dpi_tiff(tiff.percorso());
        assert!((dx - 100.0).abs() < 0.1 && (dy - 100.0).abs() < 0.1, "{dx} {dy}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
