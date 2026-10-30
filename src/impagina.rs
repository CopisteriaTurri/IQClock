//! Impaginazione sul rotolo: allinea a sinistra, al centro o a destra un
//! contenuto più stretto della carta.
//!
//! Niente comandi PJL proprietari: IQClock manda una pagina già larga quanto la
//! parte stampabile del rotolo (il rotolo meno il margine per lato), con il
//! contenuto spostato in x. Una pagina così larga la stampante non la ruota più
//! (vedi `rotazione.rs`), quindi la rotazione che farebbe lei per risparmiare
//! carta la decide qui l'app, prima di allargare.
//!
//! La pagina non deve mai essere larga quanto tutto il rotolo: provato sulla
//! T2600 il 30/10/2026, una pagina larga 914,4 mm su rotolo da 914,4 mm (alta
//! 60–210 mm) è uscita come foglio di 91 × 93 cm, cioè messa per il lungo.
//!
//! - PDF: la pagina resta com'è, si allarga solo il suo MediaBox attorno al
//!   contenuto (e /Rotate porta l'eventuale rotazione). Il contenuto non viene
//!   toccato né rasterizzato, e le annotazioni restano al loro posto; un
//!   tracciato di ritaglio sul riquadro originale nasconde quello che prima
//!   cadeva fuori pagina.
//! - JPEG, TIFF e PNG: si riscrivono in TIFF con una banda bianca fino alla
//!   larghezza della pagina, mantenendo i DPI del file. Dei TIFF multipagina
//!   parte solo la prima pagina.
//! - PostScript e HP-GL/2 partono come sono.
//!
//! Con "Sinistra", o se il contenuto non entra nella larghezza utile del rotolo,
//! il file parte com'è oggi e decide la stampante.

use std::path::Path;

use anyhow::{Context, Result, bail};
use image::RgbImage;
use lopdf::{Dictionary, Object, Stream};

use crate::file::{self, FileTemporaneo};
use crate::pjl::Formato;
use crate::png;
use crate::rotazione::{self, TOLLERANZA_MM};

const MM_PT: f64 = 72.0 / 25.4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Allineamento {
    /// Il file parte com'è: la stampante lo mette a sinistra.
    #[default]
    Sinistra,
    Centro,
    Destra,
}

impl Allineamento {
    pub const TUTTI: [Allineamento; 3] = [Allineamento::Sinistra, Allineamento::Centro, Allineamento::Destra];

    pub fn etichetta(self) -> &'static str {
        match self {
            Allineamento::Sinistra => "Sinistra",
            Allineamento::Centro => "Centro",
            Allineamento::Destra => "Destra",
        }
    }

    /// Per la riga di comando: "sinistra", "centro", "destra".
    pub fn da_testo(testo: &str) -> Option<Self> {
        Self::TUTTI.into_iter().find(|a| a.etichetta().eq_ignore_ascii_case(testo))
    }
}

/// Formati che IQClock sa impaginare; gli altri partono come sono.
pub fn impaginabile(formato: Formato) -> bool {
    !matches!(formato, Formato::PostScript | Formato::Hpgl2)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Impaginazione {
    pub allineamento: Allineamento,
    pub rotolo_mm: f64,
    /// Bordo non stampabile della stampante, per lato: la pagina mandata è
    /// larga quanto il rotolo meno questo margine sui due lati.
    pub margine_mm: f64,
}

/// Margine minimo per impaginare: con meno la pagina sarebbe larga (quasi)
/// quanto il rotolo e la stampante la metterebbe per il lungo. 3 mm è il
/// margine che la T2600 dichiara via IPP; la prova reale è stata fatta con 5.
pub const MARGINE_MIN_MM: f64 = 3.0;

/// Dove finisce il contenuto sulla pagina larga quanto la parte stampabile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Posizione {
    /// Il contenuto va ruotato di 90° in senso orario prima di allargare.
    pub ruota: bool,
    /// Distanza del contenuto dal bordo sinistro della pagina.
    pub x_mm: f64,
}

impl Impaginazione {
    /// Larghezza della pagina mandata: il rotolo meno il margine per lato.
    pub fn larghezza_pagina_mm(&self) -> f64 {
        self.rotolo_mm - 2.0 * self.margine_mm
    }

    /// Posizione di un contenuto `larghezza` × `altezza` mm (`larghezza` è il
    /// lato lungo il rotolo). `None` se il file deve partire com'è.
    pub fn posiziona(&self, larghezza: f64, altezza: f64) -> Option<Posizione> {
        if self.allineamento == Allineamento::Sinistra || self.margine_mm < MARGINE_MIN_MM {
            return None;
        }
        let ruota = rotazione::ruota(larghezza, altezza, self.rotolo_mm);
        let larghezza = if ruota { altezza } else { larghezza };
        let libero = self.larghezza_pagina_mm() - larghezza;
        if libero < -TOLLERANZA_MM {
            return None;
        }
        let libero = libero.max(0.0);
        let x_mm = match self.allineamento {
            Allineamento::Sinistra => 0.0,
            Allineamento::Centro => libero / 2.0,
            Allineamento::Destra => libero,
        };
        Some(Posizione { ruota, x_mm })
    }
}

/// Prepara il file da mandare: il file impaginato in un temporaneo, col formato
/// con cui parte (le immagini diventano TIFF), oppure `None` se parte com'è.
pub fn prepara(percorso: &Path, formato: Formato, imp: &Impaginazione) -> Result<Option<(FileTemporaneo, Formato)>> {
    match formato {
        Formato::Pdf => {
            let dati = std::fs::read(percorso)?;
            let Some(composto) = componi_pdf(&dati, imp)? else { return Ok(None) };
            let dest = FileTemporaneo::nuovo("pdf");
            std::fs::write(dest.percorso(), composto.dati).context("scrittura del PDF impaginato")?;
            Ok(Some((dest, Formato::Pdf)))
        }
        Formato::Jpeg | Formato::Tiff | Formato::Png => {
            let dpi = file::dpi_immagine(percorso, formato);
            // Prima con le sole dimensioni: decodificare serve solo se si impagina.
            let d = imagesize::size(percorso).context("dimensioni dell'immagine")?;
            if imp.posiziona(d.width as f64 * 25.4 / dpi.0, d.height as f64 * 25.4 / dpi.1).is_none() {
                return Ok(None);
            }
            let Ok(composto) = componi_raster(png::su_bianco(png::decodifica(percorso)?), dpi, imp) else {
                return Ok(None);
            };
            let dest = composto.scrivi_tiff()?;
            Ok(Some((dest, Formato::Tiff)))
        }
        Formato::PostScript | Formato::Hpgl2 => Ok(None),
    }
}

/// PDF impaginato in memoria.
pub struct PdfComposto {
    pub dati: Vec<u8>,
    /// Per ogni pagina: dove IQClock l'ha messa, `None` se è rimasta com'è.
    pub posizioni: Vec<Option<Posizione>>,
}

/// Allarga alla parte stampabile del rotolo ogni pagina che ci entra. `None` se
/// nessuna pagina cambia.
pub fn componi_pdf(dati: &[u8], imp: &Impaginazione) -> Result<Option<PdfComposto>> {
    let mut doc = lopdf::Document::load_mem(dati).context("apertura del PDF")?;
    if doc.is_encrypted() {
        bail!("PDF cifrato: non si può impaginare (scegli l'allineamento a sinistra)");
    }
    let pagina_pt = imp.larghezza_pagina_mm() * MM_PT;
    let mut posizioni = Vec::new();
    let mut cambiate = false;
    for id in doc.get_pages().into_values() {
        let Some(([x0, y0, x1, y1], ruota)) = file::box_e_rotazione(&doc, id).filter(|(_, r)| r % 90 == 0) else {
            posizioni.push(None);
            continue;
        };
        let (l, a) = ((x1 - x0) / MM_PT, (y1 - y0) / MM_PT);
        let (l, a) = if ruota % 180 == 90 { (a, l) } else { (l, a) };
        let Some(pos) = imp.posiziona(l, a) else {
            posizioni.push(None);
            continue;
        };
        posizioni.push(Some(pos));
        cambiate = true;
        let ruota = (ruota + if pos.ruota { 90 } else { 0 }) % 360;
        let dx = pos.x_mm * MM_PT;
        // Con /Rotate la pagina viene girata in senso orario: il bordo che finisce
        // a sinistra è quello basso (90°), destro (180°) o alto (270°). Il box si
        // allarga lungo il lato che diventa orizzontale, così che a sinistra del
        // contenuto restino `dx` punti e in tutto la pagina sia larga `pagina_pt`.
        let nuovo = match ruota {
            0 => [x0 - dx, y0, x0 - dx + pagina_pt, y1],
            90 => [x0, y0 - dx, x1, y0 - dx + pagina_pt],
            180 => [x1 + dx - pagina_pt, y0, x1 + dx, y1],
            _ => [x0, y1 + dx - pagina_pt, x1, y1 + dx],
        };
        let ritaglio = format!("q {x0:.4} {y0:.4} {:.4} {:.4} re W n\n", x1 - x0, y1 - y0);
        let prima = doc.add_object(Stream::new(Dictionary::new(), ritaglio.into_bytes()));
        let dopo = doc.add_object(Stream::new(Dictionary::new(), b"\nQ\n".to_vec()));
        let mut contenuti: Vec<Object> = vec![prima.into()];
        let originali = doc.get_dictionary(id)?.get(b"Contents").ok().cloned();
        match originali {
            Some(Object::Array(a)) => contenuti.extend(a),
            // Un riferimento può puntare a un array di stream.
            Some(Object::Reference(r)) => match doc.get_object(r) {
                Ok(Object::Array(a)) => contenuti.extend(a.clone()),
                _ => contenuti.push(Object::Reference(r)),
            },
            Some(o) => contenuti.push(o),
            None => {}
        }
        contenuti.push(dopo.into());
        let riquadro: Vec<Object> = nuovo.iter().map(|&v| Object::Real(v as f32)).collect();
        let pagina = doc.get_dictionary_mut(id)?;
        pagina.set("Contents", contenuti);
        pagina.set("MediaBox", riquadro.clone());
        // Il CropBox è ereditabile: va riscritto, non solo tolto.
        pagina.set("CropBox", riquadro);
        pagina.set("Rotate", ruota);
        for chiave in [&b"BleedBox"[..], b"TrimBox", b"ArtBox"] {
            pagina.remove(chiave);
        }
    }
    if !cambiate {
        return Ok(None);
    }
    let mut out = Vec::new();
    doc.save_to(&mut out).context("scrittura del PDF impaginato")?;
    Ok(Some(PdfComposto { dati: out, posizioni }))
}

/// Immagine impaginata: il contenuto (già ruotato se serve) e dove sta sul
/// rotolo. Le bande bianche si generano solo mentre si scrive.
pub struct RasterComposto {
    contenuto: RgbImage,
    pub dpi: (f64, f64),
    pub larghezza_px: u32,
    pub x_px: u32,
    pub ruotata: bool,
}

/// Impagina un'immagine ai DPI con cui verrà stampata; restituisce l'immagine
/// intatta se deve partire com'è.
pub fn componi_raster(immagine: RgbImage, dpi: (f64, f64), imp: &Impaginazione) -> Result<RasterComposto, RgbImage> {
    let (l, a) = immagine.dimensions();
    let Some(pos) = imp.posiziona(l as f64 * 25.4 / dpi.0, a as f64 * 25.4 / dpi.1) else {
        return Err(immagine);
    };
    let (contenuto, dpi) =
        if pos.ruota { (image::imageops::rotate90(&immagine), (dpi.1, dpi.0)) } else { (immagine, dpi) };
    let larghezza = contenuto.width();
    let larghezza_px = ((imp.larghezza_pagina_mm() / 25.4 * dpi.0).round() as u32).max(larghezza);
    let x_px = ((pos.x_mm / 25.4 * dpi.0).round() as u32).min(larghezza_px - larghezza);
    Ok(RasterComposto { contenuto, dpi, larghezza_px, x_px, ruotata: pos.ruota })
}

impl RasterComposto {
    pub fn altezza_px(&self) -> u32 {
        self.contenuto.height()
    }

    /// Riga `y` larga quanto la pagina, in RGB.
    fn riga(&self, y: u32, out: &mut [u8]) {
        out.fill(255);
        let byte = self.contenuto.width() as usize * 3;
        let inizio = y as usize * byte;
        let x = self.x_px as usize * 3;
        out[x..x + byte].copy_from_slice(&self.contenuto.as_raw()[inizio..inizio + byte]);
    }

    pub fn scrivi_tiff(&self) -> Result<FileTemporaneo> {
        png::tiff_temporaneo(self.larghezza_px, self.altezza_px(), self.dpi, |y, out| self.riga(y, out))
    }

    /// L'immagine intera larga quanto la pagina (per l'anteprima, a bassa risoluzione).
    pub fn su_rotolo(&self) -> RgbImage {
        let mut out = RgbImage::new(self.larghezza_px, self.altezza_px());
        let byte = self.larghezza_px as usize * 3;
        for (y, riga) in out.chunks_exact_mut(byte).enumerate() {
            self.riga(y as u32, riga);
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use lopdf::dictionary;

    fn imp(allineamento: Allineamento) -> Impaginazione {
        Impaginazione { allineamento, rotolo_mm: 914.0, margine_mm: 5.0 }
    }

    #[test]
    fn sinistra_non_tocca_niente() {
        assert_eq!(imp(Allineamento::Sinistra).posiziona(210.0, 148.0), None);
    }

    #[test]
    fn centro_e_destra_rispettano_il_margine() {
        // A5 orizzontale: resta dritto.
        let c = imp(Allineamento::Centro).posiziona(210.0, 148.0).unwrap();
        assert_eq!(c, Posizione { ruota: false, x_mm: (904.0 - 210.0) / 2.0 });
        let d = imp(Allineamento::Destra).posiziona(210.0, 148.0).unwrap();
        assert_eq!(d, Posizione { ruota: false, x_mm: 904.0 - 210.0 });
    }

    #[test]
    fn ruota_come_farebbe_la_stampante() {
        // A5 verticale: la stampante lo ruoterebbe, quindi lo ruota l'app.
        let d = imp(Allineamento::Destra).posiziona(148.0, 210.0).unwrap();
        assert_eq!(d, Posizione { ruota: true, x_mm: 904.0 - 210.0 });
        // A0 orizzontale: entra solo ruotato (841 di larghezza).
        let c = imp(Allineamento::Centro).posiziona(1189.0, 841.0).unwrap();
        assert!(c.ruota);
        assert!((c.x_mm - (904.0 - 841.0) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn senza_spazio_utile_parte_com_e() {
        // 910 mm su 914 con 5 mm di margine per lato: non entra nella parte stampabile.
        assert_eq!(imp(Allineamento::Centro).posiziona(910.0, 2000.0), None);
        assert_eq!(imp(Allineamento::Destra).posiziona(1200.0, 1000.0), None);
        // Esattamente la larghezza utile: va, attaccato al margine.
        let p = imp(Allineamento::Destra).posiziona(904.0, 2000.0).unwrap();
        assert_eq!(p.x_mm, 0.0);
    }

    #[test]
    fn mai_una_pagina_larga_quanto_il_rotolo() {
        // Margine 5: la pagina è larga 904 su un rotolo da 914.
        assert_eq!(imp(Allineamento::Centro).larghezza_pagina_mm(), 904.0);
        // Sotto il minimo non si impagina: la stampante metterebbe la pagina per
        // il lungo (provato: fogli da 91 × 93 cm).
        let zero = Impaginazione { margine_mm: 0.0, ..imp(Allineamento::Centro) };
        assert_eq!(zero.posiziona(210.0, 148.0), None);
        let minimo = Impaginazione { margine_mm: MARGINE_MIN_MM, ..imp(Allineamento::Centro) };
        assert!(minimo.posiziona(210.0, 148.0).is_some());
    }

    #[test]
    fn allineamento_da_riga_di_comando() {
        assert_eq!(Allineamento::da_testo("centro"), Some(Allineamento::Centro));
        assert_eq!(Allineamento::da_testo("DESTRA"), Some(Allineamento::Destra));
        assert_eq!(Allineamento::da_testo("alto"), None);
    }

    /// PDF di una pagina `l` × `a` pt con MediaBox che parte da `origine` e un
    /// rettangolo pieno che copre la pagina.
    pub(crate) fn pdf(l: f64, a: f64, origine: (f64, f64), ruota: i64) -> Vec<u8> {
        let mut doc = lopdf::Document::with_version("1.5");
        let pagine_id = doc.new_object_id();
        let (x0, y0) = origine;
        let contenuto = format!("0 0 0 rg {x0} {y0} {l} {a} re f");
        let contenuto = doc.add_object(Stream::new(dictionary! {}, contenuto.into_bytes()));
        let mut pagina = dictionary! {
            "Type" => "Page",
            "Parent" => pagine_id,
            "Contents" => contenuto,
        };
        if ruota != 0 {
            pagina.set("Rotate", ruota);
        }
        let pagina = doc.add_object(pagina);
        let mb: Vec<Object> = [x0, y0, x0 + l, y0 + a].iter().map(|&v| Object::Real(v as f32)).collect();
        doc.objects.insert(
            pagine_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![pagina.into()],
                "Count" => 1,
                "MediaBox" => mb,
            }),
        );
        let catalogo = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pagine_id });
        doc.trailer.set("Root", catalogo);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    /// Pagina composta: MediaBox, /Rotate e operatori di contenuto.
    fn pagina_composta(dati: &[u8]) -> ([f64; 4], i64, String) {
        let doc = lopdf::Document::load_mem(dati).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        let (mb, ruota) = file::box_e_rotazione(&doc, id).unwrap();
        let testo = String::from_utf8_lossy(&doc.get_page_content(id)).into_owned();
        (mb, ruota, testo)
    }

    /// Lato sinistro del contenuto sulla pagina come appare, in mm: distanza tra
    /// il bordo che finisce a sinistra del box nuovo e quello del contenuto.
    fn offset_mm(mb: [f64; 4], originale: [f64; 4], ruota: i64) -> (f64, f64) {
        let (dx, larghezza) = match ruota {
            0 => (originale[0] - mb[0], mb[2] - mb[0]),
            90 => (originale[1] - mb[1], mb[3] - mb[1]),
            180 => (mb[2] - originale[2], mb[2] - mb[0]),
            _ => (mb[3] - originale[3], mb[3] - mb[1]),
        };
        (dx / MM_PT, larghezza / MM_PT)
    }

    #[test]
    fn pdf_centrato_resta_vettoriale() {
        let (l, a) = (210.0 * MM_PT, 148.0 * MM_PT);
        let dati = pdf(l, a, (0.0, 0.0), 0);
        let c = componi_pdf(&dati, &imp(Allineamento::Centro)).unwrap().unwrap();
        assert_eq!(c.posizioni.iter().map(|p| p.unwrap().ruota).collect::<Vec<_>>(), vec![false]);
        let (mb, ruota, testo) = pagina_composta(&c.dati);
        assert_eq!(ruota, 0);
        let (x, larghezza) = offset_mm(mb, [0.0, 0.0, l, a], ruota);
        assert!((larghezza - 904.0).abs() < 0.01, "{larghezza}");
        assert!((x - (904.0 - 210.0) / 2.0).abs() < 0.01, "{x}");
        assert!(((mb[3] - mb[1]) / MM_PT - 148.0).abs() < 0.01, "altezza invariata");
        // Il contenuto originale c'è ancora, ritagliato sul riquadro originale.
        assert!(testo.contains("re f"), "{testo}");
        assert!(testo.trim_start().starts_with("q ") && testo.contains("re W n"), "{testo}");
        assert!(testo.trim_end().ends_with('Q'), "{testo}");
        // Nessuna immagine: niente è stato rasterizzato.
        let doc = lopdf::Document::load_mem(&c.dati).unwrap();
        assert!(!doc.objects.values().any(|o| o
            .as_stream()
            .is_ok_and(|s| s.dict.get(b"Subtype").and_then(|t| t.as_name()).is_ok_and(|n| n == b"Image"))));
    }

    #[test]
    fn pdf_verticale_ruotato_a_destra() {
        // A5 verticale con MediaBox spostato: l'app lo ruota (come la stampante)
        // e lo mette contro il margine destro.
        let (l, a) = (148.0 * MM_PT, 210.0 * MM_PT);
        let originale = [50.0, 30.0, 50.0 + l, 30.0 + a];
        let dati = pdf(l, a, (50.0, 30.0), 0);
        let c = componi_pdf(&dati, &imp(Allineamento::Destra)).unwrap().unwrap();
        assert_eq!(c.posizioni.iter().map(|p| p.unwrap().ruota).collect::<Vec<_>>(), vec![true]);
        let (mb, ruota, _) = pagina_composta(&c.dati);
        assert_eq!(ruota, 90);
        let (x, larghezza) = offset_mm(mb, originale, ruota);
        assert!((larghezza - 904.0).abs() < 0.01, "{larghezza}");
        assert!((x - (904.0 - 210.0)).abs() < 0.01, "{x}");
        // L'altezza (ora lungo la carta) è la larghezza originale.
        assert!(((mb[2] - mb[0]) / MM_PT - 148.0).abs() < 0.01);
    }

    #[test]
    fn pdf_con_rotate_esistente() {
        // Pagina 210 × 148 con /Rotate 90: appare 148 × 210 (verticale), quindi
        // si aggiunge un'altra rotazione e il totale è 180°.
        let (l, a) = (210.0 * MM_PT, 148.0 * MM_PT);
        let dati = pdf(l, a, (0.0, 0.0), 90);
        let c = componi_pdf(&dati, &imp(Allineamento::Centro)).unwrap().unwrap();
        let (mb, ruota, _) = pagina_composta(&c.dati);
        assert_eq!(ruota, 180);
        let (x, larghezza) = offset_mm(mb, [0.0, 0.0, l, a], ruota);
        assert!((larghezza - 904.0).abs() < 0.01);
        assert!((x - (904.0 - 210.0) / 2.0).abs() < 0.01, "{x}");

        // Con /Rotate 270 il totale fa 360°, cioè 0.
        let dati = pdf(l, a, (0.0, 0.0), 270);
        let c = componi_pdf(&dati, &imp(Allineamento::Destra)).unwrap().unwrap();
        let (mb, ruota, _) = pagina_composta(&c.dati);
        assert_eq!(ruota, 0);
        let (x, _) = offset_mm(mb, [0.0, 0.0, l, a], ruota);
        assert!((x - (904.0 - 210.0)).abs() < 0.01, "{x}");
    }

    #[test]
    fn pdf_troppo_largo_parte_com_e() {
        let dati = pdf(1189.0 * MM_PT, 1189.0 * MM_PT, (0.0, 0.0), 0);
        assert!(componi_pdf(&dati, &imp(Allineamento::Centro)).unwrap().is_none());
        assert!(componi_pdf(&dati, &imp(Allineamento::Sinistra)).unwrap().is_none());
    }

    #[test]
    fn raster_allargato_con_banda_bianca() {
        // 100 × 50 px a 100 dpi (25,4 × 12,7 mm) su rotolo da 100 mm, margine 5:
        // pagina da 90 mm.
        let nero = RgbImage::from_pixel(100, 50, image::Rgb([0, 0, 0]));
        let imp = Impaginazione { allineamento: Allineamento::Destra, rotolo_mm: 100.0, margine_mm: 5.0 };
        let c = componi_raster(nero, (100.0, 100.0), &imp).ok().unwrap();
        assert!(!c.ruotata);
        assert_eq!(c.larghezza_px, 354); // 90 mm a 100 dpi
        assert_eq!(c.x_px, 354 - 100); // contro il bordo destro della pagina
        let img = c.su_rotolo();
        assert_eq!(img.dimensions(), (354, 50));
        assert_eq!(img.get_pixel(c.x_px - 1, 10).0, [255, 255, 255]);
        assert_eq!(img.get_pixel(c.x_px, 10).0, [0, 0, 0]);
        assert_eq!(img.get_pixel(c.x_px + 99, 10).0, [0, 0, 0]);
        assert_eq!(img.get_pixel(c.x_px - 50, 10).0, [255, 255, 255]);
    }

    #[test]
    fn raster_verticale_ruotato_e_dpi_scambiati() {
        // 50 × 200 px a 100 × 120 dpi (12,7 × 42,3 mm): verticale, si ruota per
        // risparmiare carta e i DPI dei due assi si scambiano.
        let img = RgbImage::from_fn(50, 200, |x, _| image::Rgb([x as u8, 0, 0]));
        let imp = Impaginazione { allineamento: Allineamento::Centro, rotolo_mm: 100.0, margine_mm: 3.0 };
        let c = componi_raster(img, (100.0, 120.0), &imp).ok().unwrap();
        assert!(c.ruotata);
        assert_eq!(c.dpi, (120.0, 100.0));
        assert_eq!(c.altezza_px(), 50);
        assert_eq!(c.larghezza_px, (94.0 / 25.4 * 120.0_f64).round() as u32);
    }

    #[test]
    fn tiff_impaginato_ha_le_misure_del_rotolo() {
        let dir = std::env::temp_dir().join(format!("iqclock-impagina-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("foto.png");
        RgbImage::from_pixel(100, 50, image::Rgb([0, 0, 0])).save(&p).unwrap();
        let imp = Impaginazione { allineamento: Allineamento::Centro, rotolo_mm: 914.0, margine_mm: 5.0 };
        // Senza pHYs: 300 dpi presunti, quindi la pagina da 904 mm = 10677 px.
        let (tiff, formato) = prepara(&p, Formato::Png, &imp).unwrap().unwrap();
        assert_eq!(formato, Formato::Tiff);
        let letto = image::open(tiff.percorso()).unwrap().to_rgb8();
        assert_eq!(letto.dimensions(), (10677, 50));
        let x = (10677 - 100) / 2;
        assert_eq!(letto.get_pixel(x as u32 - 2, 0).0, [255, 255, 255]);
        assert_eq!(letto.get_pixel(x as u32 + 2, 0).0, [0, 0, 0]);
        // Con "Sinistra" nessun file nuovo.
        let sinistra = Impaginazione { allineamento: Allineamento::Sinistra, ..imp };
        assert!(prepara(&p, Formato::Png, &sinistra).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
