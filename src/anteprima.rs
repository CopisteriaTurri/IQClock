//! Rendering delle anteprime (prime pagine dei PDF con pdfium, immagini con `image`)
//! su un thread dedicato: pdfium non è thread-safe, quindi una sola istanza lo usa.
//!
//! La libreria pdfium (`pdfium.dll` / `libpdfium.so`, build chromium/7881 di
//! bblanchon/pdfium-binaries) va messa accanto all'eseguibile. Se manca, i PDF
//! restano senza anteprima e il resto funziona.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use eframe::egui::ColorImage;
use pdfium_render::prelude::*;

use crate::pjl::Formato;

/// Pagine renderizzate per documento (quante ne mostra l'anteprima sul rotolo).
pub const PAGINE_MAX: usize = 4;
/// Lato lungo massimo delle immagini di anteprima, in pixel.
const LATO_MAX: i32 = 1400;

/// Una pagina renderizzata con le sue misure (le pagine di un PDF possono
/// avere formati diversi; per le immagini le misure non si conoscono).
pub struct Pagina {
    pub immagine: ColorImage,
    pub mm: Option<(f64, f64)>,
}

pub struct Anteprime {
    richieste: Sender<(u64, PathBuf, Formato)>,
}

impl Anteprime {
    /// Avvia il thread. `consegna` riceve l'id del documento e le sue pagine.
    /// Restituisce anche l'errore di caricamento di pdfium, se c'è stato.
    pub fn avvia(consegna: impl Fn(u64, Vec<Pagina>) + Send + 'static) -> (Self, Option<String>) {
        let (richieste, rx) = channel::<(u64, PathBuf, Formato)>();
        let (esito_tx, esito_rx) = channel();
        thread::spawn(move || {
            let pdfium = carica_pdfium();
            let _ = esito_tx.send(pdfium.as_ref().err().cloned());
            for (id, percorso, formato) in rx {
                let pagine = match formato {
                    Formato::Pdf => match &pdfium {
                        Ok(p) => pagine_pdf(p, &percorso).unwrap_or_default(),
                        Err(_) => Vec::new(),
                    },
                    Formato::Jpeg | Formato::Tiff | Formato::Png => {
                        immagine(&percorso).map(|immagine| Pagina { immagine, mm: None }).into_iter().collect()
                    }
                    Formato::PostScript | Formato::Hpgl2 => Vec::new(),
                };
                consegna(id, pagine);
            }
        });
        let errore = esito_rx.recv().unwrap_or_else(|_| Some("thread anteprime terminato".into()));
        (Self { richieste }, errore)
    }

    pub fn richiedi(&self, id: u64, percorso: PathBuf, formato: Formato) {
        let _ = self.richieste.send((id, percorso, formato));
    }
}

/// Cerca pdfium accanto all'eseguibile, poi (in debug) in `pdfium/<piattaforma>`
/// del progetto, infine tra le librerie di sistema.
fn carica_pdfium() -> Result<Pdfium, String> {
    let mut cartelle: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        cartelle.push(dir);
    }
    if cfg!(debug_assertions) {
        let piattaforma = if cfg!(windows) { "win-x64" } else { "linux-x64" };
        cartelle.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("pdfium").join(piattaforma));
    }
    for dir in &cartelle {
        let lib = Pdfium::pdfium_platform_library_name_at_path(dir);
        if lib.exists() {
            return Pdfium::bind_to_library(&lib)
                .map(Pdfium::new)
                .map_err(|e| format!("{}: {e}", lib.display()));
        }
    }
    Pdfium::bind_to_system_library().map(Pdfium::new).map_err(|_| {
        format!(
            "{} non trovata accanto all'eseguibile: anteprima dei PDF non disponibile",
            Pdfium::pdfium_platform_library_name().to_string_lossy()
        )
    })
}

fn pagine_pdf(pdfium: &Pdfium, percorso: &Path) -> Result<Vec<Pagina>, PdfiumError> {
    let doc = pdfium.load_pdf_from_file(percorso, None)?;
    // Come scale_page_to_display_size, ma senza ruotare le pagine orizzontali:
    // l'anteprima deve mostrare la pagina com'è.
    let config = PdfRenderConfig::new()
        .set_target_width(LATO_MAX)
        .set_maximum_width(LATO_MAX)
        .set_maximum_height(LATO_MAX);
    let mut out = Vec::new();
    for pagina in doc.pages().iter().take(PAGINE_MAX) {
        let rgba = pagina.render_with_config(&config)?.as_image()?.to_rgba8();
        let dim = [rgba.width() as usize, rgba.height() as usize];
        out.push(Pagina {
            immagine: ColorImage::from_rgba_unmultiplied(dim, &rgba),
            mm: Some((pagina.width().to_mm() as f64, pagina.height().to_mm() as f64)),
        });
    }
    Ok(out)
}

fn immagine(percorso: &Path) -> Option<ColorImage> {
    let img = image::open(percorso).ok()?.thumbnail(LATO_MAX as u32, LATO_MAX as u32).to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], &img))
}

#[cfg(test)]
mod test {
    use super::*;
    use lopdf::dictionary;

    /// Richiede pdfium in `pdfium/linux-x64` (vedi `scarica-pdfium.sh`).
    #[test]
    fn rende_un_pdf_a1() {
        let Ok(pdfium) = carica_pdfium() else {
            eprintln!("pdfium assente: test saltato");
            return;
        };
        let pdf = std::env::temp_dir().join(format!("iqclock-a1-{}.pdf", std::process::id()));
        // PDF minimo: una pagina A1 (1684 × 2384 pt) vuota.
        let mut doc = lopdf::Document::with_version("1.5");
        let pagine_id = doc.new_object_id();
        let pagina = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pagine_id,
            "MediaBox" => vec![0.into(), 0.into(), 1684.into(), 2384.into()],
        });
        doc.objects.insert(
            pagine_id,
            lopdf::Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![pagina.into()], "Count" => 1,
            }),
        );
        let catalogo = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pagine_id });
        doc.trailer.set("Root", catalogo);
        doc.save(&pdf).unwrap();

        let pagine = pagine_pdf(&pdfium, &pdf).unwrap();
        std::fs::remove_file(&pdf).unwrap();
        assert_eq!(pagine.len(), 1);
        let [l, a] = pagine[0].immagine.size;
        assert_eq!(a, LATO_MAX as usize);
        assert!((l as f64 / a as f64 - 1684.0 / 2384.0).abs() < 0.01);
        let (l_mm, a_mm) = pagine[0].mm.unwrap();
        assert!((l_mm - 594.0).abs() < 1.0 && (a_mm - 841.0).abs() < 1.0);
    }
}
