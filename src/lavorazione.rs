//! Trasformazioni scelte per un documento prima dell'impaginazione sul rotolo.
//!
//! Per ora c'è solo la scala (#10): originale, formato ISO A0–A4, percentuale o
//! misura in mm. I PDF si scalano in vettoriale (nuovo MediaBox e una matrice
//! `cm` attorno al contenuto, annotazioni comprese); le immagini non vengono
//! ricampionate, cambiano solo i DPI dichiarati nel TIFF. PostScript e HP-GL/2
//! partono sempre come sono.
//!
//! L'ordine dei passi è quello comune a tutte le issue: prima la scala, poi
//! l'allineamento (qui rifattorizzato in `impagina::applica_pdf`).

use std::path::Path;

use anyhow::{Context, Result, bail};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::file::{self, FileTemporaneo};
use crate::impagina::{self, Impaginazione};
use crate::pjl::Formato;
use crate::png;

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

/// Trasformazioni scelte per un documento.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Lavorazione {
    pub scala: Scala,
}

impl Lavorazione {
    pub fn nulla(&self) -> bool {
        self.scala.originale()
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
        bail!("PDF cifrato: non si può ridimensionare (scegli originale)");
    }
    let mut cambiato = applica_scala(&mut doc, lav)?;
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

/// Scala una pagina in vettoriale. Le pagine senza box o con `/Rotate` non
/// multiplo di 90 restano come sono.
fn applica_scala_pagina(doc: &mut Document, id: ObjectId, scala: &Scala) -> Result<bool> {
    let Some(([x0, y0, x1, y1], ruota)) = file::box_e_rotazione(doc, id) else {
        return Ok(false);
    };
    if ruota % 90 != 0 {
        return Ok(false);
    }
    let (larghezza, altezza) = ((x1 - x0) / MM_PT, (y1 - y0) / MM_PT);
    let (larghezza, altezza) = if ruota % 180 == 90 {
        (altezza, larghezza)
    } else {
        (larghezza, altezza)
    };
    let (sx_vis, sy_vis) = fattori(scala, larghezza, altezza);
    if (sx_vis - 1.0).abs() < 1e-9 && (sy_vis - 1.0).abs() < 1e-9 {
        return Ok(false);
    }
    // Con /Rotate 90/270 la x visiva è la y dello spazio utente.
    let (sx, sy) = if ruota % 180 == 90 {
        (sy_vis, sx_vis)
    } else {
        (sx_vis, sy_vis)
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
/// Con una scala diversa dall'originale JPEG e TIFF partono come TIFF.
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
    let impagina = imp.is_some_and(|imp| imp.posiziona(larghezza * sx, altezza * sy).is_some());
    if !scala && !impagina {
        return Ok(None);
    }

    let immagine = png::su_bianco(png::decodifica(percorso)?);
    if impagina {
        let imp = imp.expect("impagina implica un'impaginazione");
        let Ok(composto) = impagina::componi_raster(immagine, dpi_scalati, imp) else {
            return Ok(None);
        };
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
            &Lavorazione { scala: Scala::Percentuale { x: 50.0, y: 50.0 } },
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
            &Lavorazione { scala: Scala::Percentuale { x: 50.0, y: 50.0 } },
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
        let lav = Lavorazione { scala: Scala::Percentuale { x: 100.0, y: 100.0 } };
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
                    scala: Scala::Formato(FormatoIso::A4)
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
    fn composto_nessuna_modifica_resta_none() {
        assert!(
            componi_pdf(&pdf_a4_con_annotazione(), &Lavorazione::default(), None)
                .unwrap()
                .is_none()
        );
    }
}
