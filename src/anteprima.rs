//! Rendering delle anteprime (prime pagine dei PDF con pdfium, immagini con `image`)
//! su un thread dedicato: pdfium non è thread-safe, quindi una sola istanza lo usa.
//!
//! La libreria pdfium (`pdfium.dll` / `libpdfium.so`, build chromium/7881 di
//! bblanchon/pdfium-binaries) può stare accanto all'eseguibile; con la feature
//! `pdfium-incluso` (usata per le release) è dentro l'eseguibile e viene estratta
//! nella cache utente. Se manca, i PDF restano senza anteprima e il resto funziona.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::thread;

use eframe::egui::ColorImage;
use pdfium_render::prelude::*;

use crate::file;
use crate::impagina::{self, Impaginazione};
use crate::lavorazione::{self, Lavorazione};
use crate::pjl::Formato;
use crate::png;

/// Pagine renderizzate per documento (quante ne mostra l'anteprima sul rotolo).
pub const PAGINE_MAX: usize = 4;
/// Pagine di cui si misurano i DPI: un disegno di 200 pagine si scandisce in
/// fretta, oltre il valore arriverebbe troppo tardi per servire.
pub const PAGINE_DPI_MAX: usize = 200;
/// Lato minimo (mm) che un'immagine deve occupare sulla carta perché i suoi DPI
/// contino: sotto ci sono loghi, timbri e simboli, su cui un avviso sarebbe rumore.
const LATO_MIN_MM: f64 = 10.0;
/// Lato lungo massimo delle immagini di anteprima, in pixel.
const LATO_MAX: i32 = 1400;

/// Una pagina renderizzata con le sue misure (le pagine di un PDF possono
/// avere formati diversi; per le immagini le misure si conoscono solo se
/// impaginate sul rotolo).
pub struct Pagina {
    pub immagine: ColorImage,
    pub mm: Option<(f64, f64)>,
    /// true se IQClock l'ha impaginata sul rotolo (pagina larga quanto la
    /// parte stampabile, che parte dal margine).
    pub impaginata: bool,
    /// true se IQClock ha ruotato la pagina di 90° per impaginarla.
    pub ruotata: bool,
    /// DPI effettivi più bassi delle immagini raster del documento (il minimo
    /// fra le pagine), alla scala scelta. `None` = nessuna immagine raster:
    /// pagina vettoriale, oppure DPI non misurati.
    pub dpi_min: Option<f64>,
    /// true se i DPI del file sono presunti (immagini senza DPI dichiarati).
    pub dpi_presunti: bool,
    /// true se i DPI sono misurati solo sulle prime [`PAGINE_DPI_MAX`] pagine.
    pub dpi_parziale: bool,
}

struct Richiesta {
    id: u64,
    percorso: PathBuf,
    formato: Formato,
    lavorazione: Lavorazione,
    impaginazione: Option<Impaginazione>,
}

/// Cosa è stato chiesto: impaginazione e lavorazione. Torna con le pagine,
/// così chi le riceve scarta i risultati di una richiesta ormai superata.
pub type Chiave = (Option<Impaginazione>, Lavorazione);

pub struct Anteprime {
    richieste: Sender<Richiesta>,
}

impl Anteprime {
    /// Avvia il thread. `consegna` riceve l'id del documento, la chiave della
    /// richiesta e le pagine.
    /// Restituisce anche l'errore di caricamento di pdfium, se c'è stato.
    pub fn avvia(consegna: impl Fn(u64, Chiave, Vec<Pagina>) + Send + 'static) -> (Self, Option<String>) {
        let (richieste, rx) = channel::<Richiesta>();
        let (esito_tx, esito_rx) = channel();
        thread::spawn(move || {
            let pdfium = carica_pdfium();
            let _ = esito_tx.send(pdfium.as_ref().err().cloned());
            while let Ok(prima) = rx.recv() {
                // Cambiando allineamento o margine arrivano richieste a raffica:
                // per ogni documento conta solo l'ultima.
                let mut ultime: Vec<Richiesta> = Vec::new();
                for r in std::iter::once(prima).chain(rx.try_iter()) {
                    ultime.retain(|u| u.id != r.id);
                    ultime.push(r);
                }
                for r in ultime {
                    consegna(
                        r.id,
                        (r.impaginazione, r.lavorazione),
                        pagine_per_richiesta(
                            &pdfium,
                            &r.percorso,
                            r.formato,
                            &r.lavorazione,
                            r.impaginazione.as_ref(),
                        ),
                    );
                }
            }
        });
        let errore = esito_rx.recv().unwrap_or_else(|_| Some("thread anteprime terminato".into()));
        (Self { richieste }, errore)
    }

    /// L'anteprima passa da scala e impaginazione, cioè è il file che parte
    /// davvero.
    pub fn richiedi(
        &self,
        id: u64,
        percorso: PathBuf,
        formato: Formato,
        lavorazione: Lavorazione,
        impaginazione: Option<Impaginazione>,
    ) {
        let _ = self.richieste.send(Richiesta { id, percorso, formato, lavorazione, impaginazione });
    }
}

/// Una richiesta del thread anteprime: i PDF senza pdfium (o illeggibili)
/// danno zero pagine ma non bloccano il resto (le immagini funzionano comunque).
fn pagine_per_richiesta(
    pdfium: &Result<Pdfium, String>,
    percorso: &Path,
    formato: Formato,
    lav: &Lavorazione,
    imp: Option<&Impaginazione>,
) -> Vec<Pagina> {
    match formato {
        Formato::Pdf => match pdfium {
            Ok(p) => pagine_pdf(p, percorso, lav, imp).unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        Formato::Jpeg | Formato::Tiff | Formato::Png => {
            immagine(percorso, formato, lav, imp).into_iter().collect()
        }
        Formato::PostScript | Formato::Hpgl2 => Vec::new(),
    }
}

/// Cerca pdfium accanto all'eseguibile, poi (in debug) in `pdfium/<piattaforma>`
/// del progetto, poi quello incluso nell'eseguibile, infine tra le librerie di sistema.
fn carica_pdfium() -> Result<Pdfium, String> {
    #[allow(unused_mut)]
    let mut cartelle = cartelle_pdfium();
    #[cfg(feature = "pdfium-incluso")]
    cartelle.push(estrai_pdfium().map_err(|e| format!("estrazione di pdfium non riuscita: {e}"))?);
    carica_pdfium_in(&cartelle, true)
}

fn cartelle_pdfium() -> Vec<PathBuf> {
    let mut cartelle: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        cartelle.push(dir);
    }
    if cfg!(debug_assertions) {
        cartelle.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("pdfium").join(PIATTAFORMA));
    }
    cartelle
}

/// Variante testabile di [`carica_pdfium`]: cerca solo in `cartelle` e, se
/// `con_fallback_sistema` è falso, non prova le librerie di sistema (così il
/// caso "libpdfium mancante" è riproducibile anche dove pdfium è installato).
fn carica_pdfium_in(cartelle: &[PathBuf], con_fallback_sistema: bool) -> Result<Pdfium, String> {
    for dir in cartelle {
        let lib = Pdfium::pdfium_platform_library_name_at_path(dir);
        if lib.exists() {
            return match Pdfium::bind_to_library(&lib) {
                Ok(bindings) => Ok(Pdfium::new(bindings)),
                // pdfium-render consente un solo binding per processo: nei test in
                // parallelo (ma non nell'app, che lo carica una volta sola) la
                // libreria può essere già inizializzata. `Pdfium::default()` in quel
                // caso riusa i binding esistenti invece di riaprire la libreria.
                Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => Ok(Pdfium::default()),
                Err(e) => Err(format!("{}: {e}", lib.display())),
            };
        }
    }
    if con_fallback_sistema {
        return match Pdfium::bind_to_system_library() {
            Ok(bindings) => Ok(Pdfium::new(bindings)),
            Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => Ok(Pdfium::default()),
            Err(_) => Err(messaggio_pdfium_mancante()),
        };
    }
    Err(messaggio_pdfium_mancante())
}

fn messaggio_pdfium_mancante() -> String {
    format!(
        "{} non trovata accanto all'eseguibile: anteprima dei PDF non disponibile",
        Pdfium::pdfium_platform_library_name().to_string_lossy()
    )
}

const PIATTAFORMA: &str = if cfg!(windows) { "win-x64" } else { "linux-x64" };

#[cfg(all(feature = "pdfium-incluso", windows))]
const PDFIUM_INCLUSO: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/pdfium/win-x64/pdfium.dll"));
#[cfg(all(feature = "pdfium-incluso", not(windows)))]
const PDFIUM_INCLUSO: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/pdfium/linux-x64/libpdfium.so"));

/// Scrive la pdfium inclusa nella cache utente (`%LOCALAPPDATA%\iqclock` su Windows,
/// `~/.cache/iqclock` altrove), se non c'è già identica, e ne restituisce la cartella.
#[cfg(feature = "pdfium-incluso")]
fn estrai_pdfium() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        })
        .ok_or_else(|| std::io::Error::other("cartella di cache non trovata"))?;
    let dir = base.join("iqclock");
    let lib = Pdfium::pdfium_platform_library_name_at_path(&dir);
    if std::fs::read(&lib).ok().as_deref() != Some(PDFIUM_INCLUSO) {
        std::fs::create_dir_all(&dir)?;
        // Prima su un file a parte: due istanze avviate insieme non vedono mai
        // una libreria scritta a metà.
        let tmp = dir.join(format!("pdfium-{}.tmp", std::process::id()));
        std::fs::write(&tmp, PDFIUM_INCLUSO)?;
        std::fs::rename(&tmp, &lib).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    }
    Ok(dir)
}

fn pagine_pdf(
    pdfium: &Pdfium,
    percorso: &Path,
    lav: &Lavorazione,
    imp: Option<&Impaginazione>,
) -> Result<Vec<Pagina>, String> {
    // Lo stesso PDF (scalato e impaginato) che `stampante.rs` manderebbe. Se non
    // si riesce a comporlo (per esempio un PDF cifrato da scalare) niente
    // anteprima: mostrare l'originale farebbe credere che parta così.
    let composto = if lav.nulla() && imp.is_none() {
        None
    } else {
        let dati = std::fs::read(percorso).map_err(|e| e.to_string())?;
        lavorazione::componi_pdf(&dati, lav, imp).map_err(|e| format!("{e:#}"))?
    };
    let carica = |e: PdfiumError| e.to_string();
    // I DPI si misurano sul file originale: la pagina composta da scala e
    // allineamento tiene il contenuto in un Form XObject, e lì pdfium non vede
    // più le immagini. Sulle pagine originali il fattore di scala si applica
    // qui, così il valore è già quello che uscirà. Il documento originale si
    // chiude prima di aprire quello da disegnare: due PDF aperti insieme per
    // niente sono memoria sprecata.
    let dpi_originale = match &composto {
        Some(c) => {
            let originale = pdfium.load_pdf_from_file(percorso, None).map_err(carica)?;
            Some(dpi_documento(&originale, &c.fattori_scala))
        }
        None => None,
    };
    let (doc, posizioni) = match &composto {
        Some(c) => (
            pdfium.load_pdf_from_byte_slice(&c.dati, None).map_err(carica)?,
            c.posizioni.as_slice(),
        ),
        None => (pdfium.load_pdf_from_file(percorso, None).map_err(carica)?, &[][..]),
    };
    // Senza scala né allineamento il documento aperto è già l'originale.
    let (dpi_min, dpi_parziale) = dpi_originale.unwrap_or_else(|| dpi_documento(&doc, &[]));
    // Come scale_page_to_display_size, ma senza ruotare le pagine orizzontali:
    // l'anteprima deve mostrare la pagina com'è.
    let config = PdfRenderConfig::new()
        .set_target_width(LATO_MAX)
        .set_maximum_width(LATO_MAX)
        .set_maximum_height(LATO_MAX);
    let mut out = Vec::new();
    for (n, pagina) in doc.pages().iter().take(PAGINE_MAX).enumerate() {
        let rgba = pagina
            .render_with_config(&config)
            .map_err(carica)?
            .as_image()
            .map_err(carica)?
            .to_rgba8();
        let dim = [rgba.width() as usize, rgba.height() as usize];
        out.push(Pagina {
            immagine: ColorImage::from_rgba_unmultiplied(dim, &rgba),
            mm: Some((pagina.width().to_mm() as f64, pagina.height().to_mm() as f64)),
            impaginata: posizioni.get(n).is_some_and(Option::is_some),
            ruotata: posizioni.get(n).copied().flatten().is_some_and(|p| p.ruota),
            dpi_min,
            dpi_presunti: false,
            dpi_parziale,
        });
    }
    Ok(out)
}

/// DPI effettivi più bassi (alla scala scelta) fra le immagini raster del PDF, e
/// se ci si è fermati a [`PAGINE_DPI_MAX`] pagine. `None` = nessuna immagine
/// abbastanza grande: pagina solo vettoriale.
fn dpi_documento(doc: &PdfDocument<'_>, fattori_scala: &[(f64, f64)]) -> (Option<f64>, bool) {
    let mut minimo: Option<f64> = None;
    for (n, pagina) in doc.pages().iter().enumerate() {
        if n >= PAGINE_DPI_MAX {
            return (minimo, true);
        }
        // Fattori effettivi nello spazio utente, già scambiati su /Rotate
        // 90/270 dalla composizione. Una pagina scartata resta a scala 1.
        let (sx, sy) = fattori_scala.get(n).copied().unwrap_or((1.0, 1.0));
        // FPDF_GetPageBoundingBox restituisce l'intersezione MediaBox/CropBox
        // nello spazio utente originale, come i bounds degli oggetti.
        let visibile = pagina.boundaries().bounding().ok().map(|b| b.bounds);
        for oggetto in pagina.objects().iter() {
            let PdfPageObject::Image(immagine) = oggetto else { continue };
            let Ok(riquadro) = immagine.bounds() else { continue };
            if visibile.is_some_and(|v| {
                riquadro.right() <= v.left() || riquadro.left() >= v.right()
                    || riquadro.top() <= v.bottom() || riquadro.bottom() >= v.top()
            }) {
                continue;
            }
            let (l, a) = (
                f64::from(riquadro.width().to_mm()),
                f64::from(riquadro.height().to_mm()),
            );
            // Conta la misura che l'immagine avrà sulla carta: un timbro di 8 mm
            // ingrandito al 200 % esce a 16 mm e i suoi DPI contano.
            if l * sx < LATO_MIN_MM || a * sy < LATO_MIN_MM {
                continue;
            }
            let (Ok(px), Ok(py)) = (immagine.width(), immagine.height()) else { continue };
            if px <= 0 || py <= 0 {
                continue;
            }
            let Ok(matrice) = immagine.matrix() else { continue };
            // La matrice porta il quadrato unitario dell'immagine sulla pagina.
            // Ogni asse pixel segue il proprio vettore, anche ruotato o inclinato;
            // la scala pagina agisce sulle componenti prima di misurare la lunghezza.
            let lato_x = (f64::from(matrice.a()) * sx).hypot(f64::from(matrice.b()) * sy);
            let lato_y = (f64::from(matrice.c()) * sx).hypot(f64::from(matrice.d()) * sy);
            if !(lato_x.is_finite() && lato_y.is_finite() && lato_x > 0.0 && lato_y > 0.0) {
                continue;
            }
            let dpi = file::dpi_effettivi(px as u32, lato_x * 25.4 / 72.0)
                .min(file::dpi_effettivi(py as u32, lato_y * 25.4 / 72.0));
            minimo = Some(minimo.map_or(dpi, |m: f64| m.min(dpi)));
        }
    }
    (minimo, false)
}

/// Anteprima di un'immagine. La scala cambia solo i DPI (i pixel restano) e,
/// se c'è, l'impaginazione passa dalla stessa `componi_raster` della stampa.
fn immagine(
    percorso: &Path,
    formato: Formato,
    lav: &Lavorazione,
    imp: Option<&Impaginazione>,
) -> Option<Pagina> {
    let originale = image::open(percorso).ok()?;
    let ridotta = originale.thumbnail(LATO_MAX as u32, LATO_MAX as u32);
    let colori = |img: &image::RgbaImage| {
        ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img)
    };
    let risoluzione = file::risoluzione_immagine(percorso, formato);
    let (dpi_x, dpi_y) = (risoluzione.x, risoluzione.y);
    let (larghezza, altezza) = (
        originale.width() as f64 * 25.4 / dpi_x,
        originale.height() as f64 * 25.4 / dpi_y,
    );
    let (sx, sy) = lavorazione::fattori(&lav.scala, larghezza, altezza);
    // La cornice fa parte del pezzo: la misura cresce come quella che parte.
    let cornice = lav.cornice.as_ref();
    let ingombro = cornice.map_or(0.0, |c| 2.0 * c.ingombro_mm());
    let mm = Some((larghezza * sx + ingombro, altezza * sy + ingombro));
    // DPI della miniatura: quelli del file, scalati come i pixel e dalla scala.
    let dpi = (
        dpi_x * ridotta.width() as f64 / originale.width() as f64 / sx,
        dpi_y * ridotta.height() as f64 / originale.height() as f64 / sy,
    );
    // I DPI effettivi sono quelli del file divisi per la scala (la pagina non
    // viene ricampionata).
    let dpi_min = Some((dpi_x / sx).min(dpi_y / sy));
    let ridotta = png::su_bianco(ridotta);
    // Senza impaginazione il pezzo esce della sua misura: con la cornice la si
    // disegna con la stessa funzione che scrive il TIFF che parte.
    let senza_rotolo = |ridotta: image::RgbImage| -> image::RgbImage {
        let img = match cornice {
            Some(c) => impagina::RasterComposto::con_cornice(ridotta, dpi, c).su_rotolo(),
            None => ridotta,
        };
        deforma(img, sx / dpi_x, sy / dpi_y)
    };
    let Some(imp) = imp else {
        return Some(Pagina {
            immagine: colori(&image::DynamicImage::ImageRgb8(senza_rotolo(ridotta)).to_rgba8()),
            mm,
            impaginata: false,
            ruotata: false,
            dpi_min,
            dpi_presunti: risoluzione.presunta,
            dpi_parziale: false,
        });
    };
    match impagina::componi_raster(ridotta, dpi, imp, cornice) {
        Ok(c) => {
            let mm = (c.larghezza_px as f64 * 25.4 / c.dpi.0, c.altezza_px() as f64 * 25.4 / c.dpi.1);
            // Pixel a DPI diversi su x e y: prima le proporzioni di stampa, poi la riduzione.
            let rotolo = deforma(c.su_rotolo(), 1.0 / c.dpi.0, 1.0 / c.dpi.1);
            let rotolo = image::DynamicImage::ImageRgb8(rotolo).thumbnail(LATO_MAX as u32, LATO_MAX as u32);
            Some(Pagina {
                immagine: colori(&rotolo.to_rgba8()),
                mm: Some(mm),
                impaginata: true,
                ruotata: c.ruotata,
                dpi_min,
                dpi_presunti: risoluzione.presunta,
                dpi_parziale: false,
            })
        }
        Err(ridotta) => Some(Pagina {
            immagine: colori(&image::DynamicImage::ImageRgb8(senza_rotolo(ridotta)).to_rgba8()),
            mm,
            impaginata: false,
            ruotata: false,
            dpi_min,
            dpi_presunti: risoluzione.presunta,
            dpi_parziale: false,
        }),
    }
}

/// Miniatura con le proporzioni di stampa. `fx` e `fy` sono i mm per pixel a
/// meno di una costante (scala diviso DPI): con valori diversi l'immagine esce
/// deformata (percentuale o misura senza proporzioni, oppure DPI diversi su x e
/// y), e così deve vederla l'anteprima. Il lato lungo resta quello della miniatura.
fn deforma(img: image::RgbImage, fx: f64, fy: f64) -> image::RgbImage {
    if (fx / fy - 1.0).abs() < 1e-6 {
        return img;
    }
    let (l, a) = (img.width() as f64 * fx, img.height() as f64 * fy);
    let k = img.width().max(img.height()) as f64 / l.max(a);
    let (l, a) = ((l * k).round().max(1.0) as u32, (a * k).round().max(1.0) as u32);
    image::imageops::resize(&img, l, a, image::imageops::FilterType::Triangle)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::impagina::Allineamento;
    use crate::lavorazione::{FormatoIso, Scala};
    use lopdf::dictionary;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static CONTATORE: AtomicU64 = AtomicU64::new(0);
    fn file_temp(suffisso: &str) -> PathBuf {
        let n = CONTATORE.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("iqclock-test-{}-{n}{suffisso}", std::process::id()))
    }

    /// PDF minimo con una pagina per ogni MediaBox `(larghezza, altezza)` in pt.
    fn crea_pdf(pagine_pt: &[(i32, i32)]) -> PathBuf {
        let mut doc = lopdf::Document::with_version("1.5");
        let pagine_id = doc.new_object_id();
        let mut kids = Vec::new();
        for &(l, a) in pagine_pt {
            let pagina = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pagine_id,
                "MediaBox" => vec![0.into(), 0.into(), l.into(), a.into()],
            });
            kids.push(pagina.into());
        }
        let n = pagine_pt.len() as i64;
        doc.objects.insert(
            pagine_id,
            lopdf::Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => kids, "Count" => lopdf::Object::Integer(n),
            }),
        );
        let catalogo = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pagine_id });
        doc.trailer.set("Root", catalogo);
        let pdf = file_temp(".pdf");
        doc.save(&pdf).unwrap();
        pdf
    }

    /// PDF A4 con un'immagine grigia 300 × 300 px disegnata su un riquadro di
    /// `larghezza_pt` × `altezza_pt` punti (a 72 pt un pollice, 300 dpi) e
    /// l'eventuale `/Rotate` di pagina in gradi.
    fn pdf_con_immagine(larghezza_pt: f64, altezza_pt: f64, ruota: i64) -> PathBuf {
        pdf_con_raster(300, 300, &format!("{larghezza_pt} 0 0 {altezza_pt} 20 20"), ruota)
    }

    fn pdf_con_raster(px: usize, py: usize, matrice: &str, ruota: i64) -> PathBuf {
        let mut doc = lopdf::Document::with_version("1.5");
        let pagine_id = doc.new_object_id();
        let immagine = doc.add_object(lopdf::Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => px as i64,
                "Height" => py as i64,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8,
            },
            vec![0x80; px * py],
        ));
        let contenuto = doc.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            format!("q {matrice} cm /Im0 Do Q").into_bytes(),
        ));
        let pagina = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pagine_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Rotate" => ruota,
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => immagine } },
            "Contents" => contenuto,
        });
        doc.objects.insert(
            pagine_id,
            lopdf::Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![pagina.into()], "Count" => 1,
            }),
        );
        let catalogo = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pagine_id });
        doc.trailer.set("Root", catalogo);
        let pdf = file_temp(".pdf");
        doc.save(&pdf).unwrap();
        pdf
    }

    /// Pdfium caricato davvero, oppure `None` con avviso (sviluppo locale senza
    /// `scarica-pdfium.sh`). In CI la libreria deve esserci: lì l'assenza è un
    /// errore, altrimenti i test di rendering passerebbero senza girare.
    fn pdfium_o_salta() -> Option<Pdfium> {
        match carica_pdfium() {
            Ok(p) => Some(p),
            Err(e) if std::env::var_os("CI").is_some() => panic!("pdfium non caricato in CI: {e}"),
            Err(e) => {
                eprintln!("pdfium assente ({e}): test saltato");
                None
            }
        }
    }

    #[test]
    fn rende_un_pdf_a1() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        // Una pagina A1 (1684 × 2384 pt) vuota.
        let pdf = crea_pdf(&[(1684, 2384)]);

        let pagine = pagine_pdf(&pdfium, &pdf, &Lavorazione::default(), None).unwrap();
        std::fs::remove_file(&pdf).unwrap();
        assert_eq!(pagine.len(), 1);
        let [l, a] = pagine[0].immagine.size;
        assert_eq!(a, LATO_MAX as usize);
        assert!((l as f64 / a as f64 - 1684.0 / 2384.0).abs() < 0.01);
        let (l_mm, a_mm) = pagine[0].mm.unwrap();
        assert!((l_mm - 594.0).abs() < 1.0 && (a_mm - 841.0).abs() < 1.0);
    }

    /// Verifica dimensioni e proporzioni dell’anteprima di un A1 adattato ad A4
    /// senza allineamento sul rotolo.
    #[test]
    fn anteprima_di_un_pdf_scalato() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        // A1 (1684 × 2384 pt) adattata ad A4: 209,8 × 297 mm.
        let pdf = crea_pdf(&[(1684, 2384)]);
        let lav = Lavorazione { scala: Scala::Formato(FormatoIso::A4), ..Default::default() };
        let pagine = pagine_pdf(&pdfium, &pdf, &lav, None).unwrap();
        std::fs::remove_file(&pdf).unwrap();
        assert_eq!(pagine.len(), 1);
        let (l_mm, a_mm) = pagine[0].mm.unwrap();
        assert!((l_mm - 210.0).abs() < 1.0 && (a_mm - 297.0).abs() < 1.0, "{l_mm} × {a_mm}");
        let [l, a] = pagine[0].immagine.size;
        assert!((l as f64 / a as f64 - 210.0 / 297.0).abs() < 0.01);
        // Non è un'impaginazione sul rotolo: la pagina resta dov'è.
        assert!(!pagine[0].impaginata && !pagine[0].ruotata);
    }

    /// Verifica i DPI dei raster nel PDF dopo scala e rotazione, escludendo
    /// i timbri troppo piccoli e le pagine solo vettoriali.
    #[test]
    fn dpi_delle_immagini_di_un_pdf() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        // 300 × 300 px su un pollice quadro: 300 dpi.
        let pdf = pdf_con_immagine(72.0, 72.0, 0);
        let pagine = pagine_pdf(&pdfium, &pdf, &Lavorazione::default(), None).unwrap();
        let dpi = pagine[0].dpi_min.expect("immagine raster nel PDF");
        assert!((dpi - 300.0).abs() < 0.5, "{dpi}");
        assert!(!pagine[0].dpi_presunti && !pagine[0].dpi_parziale);

        // Al 200 % la pagina esce grande il doppio, con metà dei DPI.
        let lav = Lavorazione { scala: Scala::Percentuale { x: 200.0, y: 200.0 }, ..Default::default() };
        let pagine = pagine_pdf(&pdfium, &pdf, &lav, None).unwrap();
        let dpi = pagine[0].dpi_min.expect("immagine raster nel PDF");
        assert!((dpi - 150.0).abs() < 0.5, "{dpi}");
        std::fs::remove_file(&pdf).unwrap();

        // Riquadro sotto i 10 mm: è un timbro, non conta.
        let timbro = pdf_con_immagine(20.0, 20.0, 0);
        let pagine = pagine_pdf(&pdfium, &timbro, &Lavorazione::default(), None).unwrap();
        assert_eq!(pagine[0].dpi_min, None);
        // Ma ingrandito al 200 % esce a 14 mm, e allora conta: 2,5 volte i 300
        // dpi presunti da 20 pt (300 px su 7 mm), diviso 2 per la scala.
        let pagine = pagine_pdf(&pdfium, &timbro, &lav, None).unwrap();
        let dpi = pagine[0].dpi_min.expect("timbro ingrandito");
        assert!((dpi - 540.0).abs() < 1.0, "{dpi}");
        std::fs::remove_file(&timbro).unwrap();

        // Pagina ruotata di 90° e scala senza proporzioni: la x della pagina
        // (orizzontale di 72 pt, 300 dpi) prende il fattore della y visiva, che
        // al 200 % è 2. Confrontando gli assi al contrario verrebbe 300.
        let ruotato = pdf_con_immagine(72.0, 36.0, 90);
        let lav = Lavorazione { scala: Scala::Percentuale { x: 100.0, y: 200.0 }, ..Default::default() };
        let pagine = pagine_pdf(&pdfium, &ruotato, &lav, None).unwrap();
        let dpi = pagine[0].dpi_min.expect("immagine raster nel PDF");
        assert!((dpi - 150.0).abs() < 0.5, "{dpi}");
        std::fs::remove_file(&ruotato).unwrap();

        // Pagina solo vettoriale: nessun DPI da mostrare.
        let vuoto = crea_pdf(&[(595, 842)]);
        let pagine = pagine_pdf(&pdfium, &vuoto, &Lavorazione::default(), None).unwrap();
        assert_eq!(pagine[0].dpi_min, None);
        std::fs::remove_file(&vuoto).unwrap();
    }

    /// Verifica che i DPI cambino soltanto per le pagine effettivamente scalate,
    /// senza applicare il fattore alle rotazioni non supportate.
    #[test]
    fn dpi_pdf_usano_solo_la_scala_applicata() {
        let Some(pdfium) = pdfium_o_salta() else {
            return;
        };
        let pdf = pdf_con_immagine(72.0, 72.0, 45);
        let lav = Lavorazione {
            scala: Scala::Percentuale { x: 200.0, y: 200.0 },
            ..Default::default()
        };
        // Rotazione non supportata: nessun PDF composto, resta a 300 dpi.
        let pagine = pagine_pdf(&pdfium, &pdf, &lav, None).unwrap();
        assert!((pagine[0].dpi_min.unwrap() - 300.0).abs() < 0.5);

        let mut doc = lopdf::Document::load(&pdf).unwrap();
        let id = *doc.get_pages().values().next().unwrap();
        let mut seconda = doc.get_dictionary(id).unwrap().clone();
        seconda.set("Rotate", 0);
        let contenuto = doc.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            b"q 36 0 0 36 20 20 cm /Im0 Do Q".to_vec(),
        ));
        seconda.set("Contents", contenuto);
        let seconda_id = doc.add_object(seconda);
        let padre = doc
            .get_dictionary(id)
            .unwrap()
            .get(b"Parent")
            .unwrap()
            .as_reference()
            .unwrap();
        let pagine = doc.get_dictionary_mut(padre).unwrap();
        pagine.set(
            "Kids",
            vec![lopdf::Object::Reference(id), seconda_id.into()],
        );
        pagine.set("Count", 2);
        doc.save(&pdf).unwrap();
        // Solo la seconda pagina viene scalata: 600 -> 300 dpi.
        // La prima resta a 300, quindi il minimo è 300, non 150.
        let pagine = pagine_pdf(&pdfium, &pdf, &lav, None).unwrap();
        assert_eq!(pagine.len(), 2);
        assert!((pagine[0].dpi_min.unwrap() - 300.0).abs() < 0.5);
        std::fs::remove_file(pdf).unwrap();
    }

    /// Verifica i DPI lungo gli assi trasformati del raster, anche con rotazione
    /// della pagina e scala diversa sui due assi.
    #[test]
    fn dpi_pdf_seguono_gli_assi_dell_immagine() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        for (matrice, ruota, x, y, attesi) in [
            ("0 72 -115.2 0 200 20", 0, 100.0, 100.0, 187.5),
            ("0 72 -115.2 0 200 20", 0, 200.0, 100.0, 93.75),
            // /Rotate scambia gli assi della scala; la CTM ruota il raster.
            ("0 72 -115.2 0 200 20", 90, 100.0, 200.0, 93.75),
            // Immagine a 45°: conta la lunghezza del vettore, non il suo bounds.
            ("72 72 -115.2 115.2 200 20", 0, 100.0, 100.0, 187.5 / 2.0_f64.sqrt()),
        ] {
            let pdf = pdf_con_raster(400, 300, matrice, ruota);
            let lav = Lavorazione { scala: Scala::Percentuale { x, y }, ..Default::default() };
            let pagine = pagine_pdf(&pdfium, &pdf, &lav, None).unwrap();
            let dpi = pagine[0].dpi_min.unwrap();
            assert!((dpi - attesi).abs() < 0.5, "{dpi} invece di {attesi}");
            assert!(file::DpiStampa { dpi, presunti: false }.avviso(200.0).is_some());
            std::fs::remove_file(pdf).unwrap();
        }
    }

    /// Verifica che i raster completamente fuori dal box visibile non influiscano
    /// sui DPI, mentre quelli parzialmente visibili continuino a contare.
    #[test]
    fn dpi_pdf_ignorano_solo_immagini_interamente_fuori_pagina() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        for ruota in [0, 90] {
            for (posizione, attesi) in [("250 400", None), ("190 120", Some(75.0))] {
                let pdf = pdf_con_raster(300, 300, &format!("288 0 0 288 {posizione}"), ruota);
                let mut doc = lopdf::Document::load(&pdf).unwrap();
                let id = *doc.get_pages().values().next().unwrap();
                // CropBox decentrato ed ereditato: i bounds restano nello
                // spazio utente, anche con /Rotate o con scala di stampa.
                let padre = doc.get_dictionary(id).unwrap().get(b"Parent").unwrap().as_reference().unwrap();
                doc.get_dictionary_mut(padre).unwrap().set(
                    "CropBox", vec![100.into(), 100.into(), 200.into(), 200.into()],
                );
                doc.save(&pdf).unwrap();
                for scala in [100.0, 200.0] {
                    let lav = Lavorazione { scala: Scala::Percentuale { x: scala, y: scala }, ..Default::default() };
                    let pagine = pagine_pdf(&pdfium, &pdf, &lav, None).unwrap();
                    match (pagine[0].dpi_min, attesi) {
                        (None, None) => {},
                        (Some(dpi), Some(attesi)) => assert!((dpi - attesi * 100.0 / scala).abs() < 0.5),
                        esito => panic!("{esito:?}, rotazione {ruota}, scala {scala}"),
                    }
                }
                std::fs::remove_file(pdf).unwrap();
            }
        }
        // Senza CropBox esplicito il limite è il MediaBox.
        let pdf = pdf_con_raster(300, 300, "288 0 0 288 700 900", 0);
        let pagine = pagine_pdf(&pdfium, &pdf, &Lavorazione::default(), None).unwrap();
        assert_eq!(pagine[0].dpi_min, None);
        std::fs::remove_file(pdf).unwrap();
    }

    /// Verifica che la scala aggiorni i DPI dell’anteprima e distingua
    /// la risoluzione dichiarata da quella presunta.
    #[test]
    fn dpi_effettivi_di_un_immagine() {
        // 1181 px dichiarati a 300 dpi (11811 px/m): 100 mm. Al 200 % escono
        // 150 dpi, senza ricampionare.
        let p = file_temp(".png");
        image::RgbImage::new(1181, 1181).save(&p).unwrap();
        std::fs::write(&p, crate::png::test::con_phys(&std::fs::read(&p).unwrap(), 11811)).unwrap();
        let pagina = immagine(&p, Formato::Png, &Lavorazione::default(), None).unwrap();
        assert!((pagina.dpi_min.unwrap() - 300.0).abs() < 1.0);
        assert!(!pagina.dpi_presunti && !pagina.dpi_parziale);
        let lav = Lavorazione { scala: Scala::Percentuale { x: 200.0, y: 200.0 }, ..Default::default() };
        let pagina = immagine(&p, Formato::Png, &lav, None).unwrap();
        assert!((pagina.dpi_min.unwrap() - 150.0).abs() < 1.0);
        std::fs::remove_file(&p).unwrap();

        // Senza DPI dichiarati valgono i 300 presunti, e l'indicatore lo dice.
        let p = png_valido(600, 300);
        let pagina = immagine(&p, Formato::Png, &Lavorazione::default(), None).unwrap();
        assert!(pagina.dpi_presunti);
        assert!((pagina.dpi_min.unwrap() - 300.0).abs() < 1.0);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn mostra_solo_le_prime_quattro_pagine() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        // Sei pagine A4 vuote: l'anteprima ne mostra al massimo PAGINE_MAX.
        let pdf = crea_pdf(&[(595, 842); 6]);
        let pagine = pagine_pdf(&pdfium, &pdf, &Lavorazione::default(), None).unwrap();
        std::fs::remove_file(&pdf).unwrap();
        assert_eq!(pagine.len(), PAGINE_MAX);
    }

    /// Verifica che PDF mancanti o corrotti producano un errore anche quando
    /// è richiesta la scala, senza ripiegare sul file originale.
    #[test]
    fn pdf_corrotto_e_inesistente_danno_errore() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        let corrotto = file_temp(".pdf");
        std::fs::write(&corrotto, b"%PDF-1.5\nnon un pdf vero").unwrap();
        assert!(pagine_pdf(&pdfium, &corrotto, &Lavorazione::default(), None).is_err(), "PDF corrotto");
        std::fs::remove_file(&corrotto).unwrap();
        assert!(
            pagine_pdf(&pdfium, &file_temp(".pdf"), &Lavorazione::default(), None).is_err(),
            "PDF inesistente"
        );
        // Da ridimensionare ma illeggibile: errore, non l'originale.
        let scalato = Lavorazione {
            scala: Scala::Percentuale { x: 50.0, y: 50.0 },
            ..Default::default()
        };
        let corrotto = file_temp(".pdf");
        std::fs::write(&corrotto, b"%PDF-1.5\nnon un pdf vero").unwrap();
        assert!(pagine_pdf(&pdfium, &corrotto, &scalato, None).is_err(), "scala di un PDF illeggibile");
        std::fs::remove_file(&corrotto).unwrap();
    }

    /// Senza libpdfium il messaggio dice cosa manca e che i PDF restano senza
    /// anteprima; `carica_pdfium_in` con fallback disattivato lo rende
    /// riproducibile anche dove pdfium è installato.
    #[test]
    fn pdfium_mancante_messaggio_chiaro() {
        let inesistente = Path::new(env!("CARGO_MANIFEST_DIR")).join("pdfium-cella-inesistente");
        let Err(msg) = carica_pdfium_in(std::slice::from_ref(&inesistente), false) else {
            panic!("una cartella inesistente non può dare pdfium");
        };
        assert!(msg.contains("anteprima dei PDF non disponibile"), "{msg}");
        assert!(msg.contains(&*Pdfium::pdfium_platform_library_name().to_string_lossy()), "{msg}");
    }

    fn png_valido(l: u32, a: u32) -> PathBuf {
        let p = file_temp(".png");
        image::RgbImage::new(l, a).save(&p).unwrap();
        p
    }

    /// Senza pdfium i PDF danno zero pagine ma le immagini funzionano comunque
    /// (l'app resta utilizzabile) e gli altri formati danno zero pagine.
    #[test]
    fn senza_pdfium_pdf_vuoto_ma_immagini_funzionano() {
        let senza: Result<Pdfium, String> = Err(messaggio_pdfium_mancante());
        let pdf = crea_pdf(&[(595, 842)]);
        assert!(
            pagine_per_richiesta(&senza, &pdf, Formato::Pdf, &Lavorazione::default(), None).is_empty()
        );
        std::fs::remove_file(&pdf).unwrap();
        assert!(
            pagine_per_richiesta(
                &senza,
                &file_temp(".pdf"),
                Formato::PostScript,
                &Lavorazione::default(),
                None
            )
            .is_empty()
        );
        assert!(
            pagine_per_richiesta(
                &senza,
                &file_temp(".pdf"),
                Formato::Hpgl2,
                &Lavorazione::default(),
                None
            )
            .is_empty()
        );

        let png = png_valido(60, 40);
        let pagine = pagine_per_richiesta(&senza, &png, Formato::Png, &Lavorazione::default(), None);
        std::fs::remove_file(&png).unwrap();
        assert_eq!(pagine.len(), 1);
        // Le misure ci sono sempre, anche senza impaginazione: 60 × 40 px a
        // 300 dpi presunti.
        let (l, a) = pagine[0].mm.unwrap();
        assert!((l - 60.0 * 25.4 / 300.0).abs() < 0.01 && (a - 40.0 * 25.4 / 300.0).abs() < 0.01);
    }

    #[test]
    fn immagine_corrotta_da_zero_pagine() {
        let p = file_temp(".png");
        std::fs::write(&p, b"non un'immagine").unwrap();
        assert!(immagine(&p, Formato::Png, &Lavorazione::default(), None).is_none());
        let senza: Result<Pdfium, String> = Err(messaggio_pdfium_mancante());
        assert!(
            pagine_per_richiesta(&senza, &p, Formato::Png, &Lavorazione::default(), None).is_empty()
        );
        assert!(
            pagine_per_richiesta(&senza, &p, Formato::Jpeg, &Lavorazione::default(), None).is_empty()
        );
        assert!(
            pagine_per_richiesta(&senza, &p, Formato::Tiff, &Lavorazione::default(), None).is_empty()
        );
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn immagine_grande_viene_ridotta_mantenendo_le_proporzioni() {
        let p = png_valido(2000, 1000);
        let img = immagine(&p, Formato::Png, &Lavorazione::default(), None).expect("PNG valido");
        std::fs::remove_file(&p).unwrap();
        let [l, a] = img.immagine.size;
        assert_eq!(l, LATO_MAX as usize);
        assert_eq!(a, LATO_MAX as usize / 2);
    }

    /// Verifica che scala libera e dimensioni fisiche producano la stessa
    /// deformazione nell’anteprima, rispettando il limite dei pixel.
    #[test]
    fn immagine_con_proporzioni_libere_esce_deformata() {
        // 600 × 300 px al 200 % × 50 %: in stampa 1200 × 150, cioè 8:1, non 2:1.
        let p = png_valido(600, 300);
        let lav = Lavorazione { scala: Scala::Percentuale { x: 200.0, y: 50.0 }, ..Default::default() };
        let img = immagine(&p, Formato::Png, &lav, None).expect("PNG valido");
        std::fs::remove_file(&p).unwrap();
        let [l, a] = img.immagine.size;
        assert!(l.max(a) <= LATO_MAX as usize);
        assert!((l as f64 / a as f64 - 8.0).abs() < 0.1, "{l} × {a}");
        let (l_mm, a_mm) = img.mm.unwrap();
        assert!((l_mm / a_mm - 8.0).abs() < 0.01);
    }

    #[test]
    fn immagine_con_dpi_diversi_su_x_e_y() {
        // 600 × 300 px a 300 × 150 dpi: in stampa 50,8 × 50,8 mm, quadrata.
        let p = png_valido(600, 300);
        let dati = crate::png::test::con_phys_xy(&std::fs::read(&p).unwrap(), 11811, 5906);
        std::fs::write(&p, dati).unwrap();
        let img = immagine(&p, Formato::Png, &Lavorazione::default(), None).expect("PNG valido");
        std::fs::remove_file(&p).unwrap();
        let [l, a] = img.immagine.size;
        assert!((l as f64 / a as f64 - 1.0).abs() < 0.01, "{l} × {a}");
        let (l_mm, a_mm) = img.mm.unwrap();
        assert!((l_mm - 50.8).abs() < 0.1 && (a_mm - 50.8).abs() < 0.1, "{l_mm} × {a_mm}");
    }

    /// Verifica che il centraggio mantenga le proporzioni fisiche con DPI diversi
    /// sui due assi o con una scala che deforma il contenuto.
    #[test]
    fn immagine_impaginata_con_dpi_diversi_o_scala_libera() {
        // 600 × 300 px al centro di una pagina da 190 mm. Prima a 300 × 150 dpi
        // (50,8 × 50,8 mm), poi a 300 dpi al 100 % × 50 % (50,8 × 12,7 mm): in
        // tutti e due i casi i pixel composti hanno DPI diversi su x e y.
        let imp = Impaginazione { allineamento: Allineamento::Centro, rotolo_mm: 200.0, margine_mm: 5.0 };
        let libera = Lavorazione { scala: Scala::Percentuale { x: 100.0, y: 50.0 }, ..Default::default() };
        for (phys, lav, alta) in [((11811, 5906), Lavorazione::default(), 50.8), ((11811, 11811), libera, 12.7)] {
            let p = png_valido(600, 300);
            let dati = crate::png::test::con_phys_xy(&std::fs::read(&p).unwrap(), phys.0, phys.1);
            std::fs::write(&p, dati).unwrap();
            let pagina = immagine(&p, Formato::Png, &lav, Some(&imp)).unwrap();
            std::fs::remove_file(&p).unwrap();
            let (l_mm, a_mm) = pagina.mm.unwrap();
            assert!((l_mm - 190.0).abs() < 0.5 && (a_mm - alta).abs() < 0.5, "{l_mm} × {a_mm}");
            let [lp, ap] = pagina.immagine.size;
            let (atteso, reale) = (l_mm / a_mm, lp as f64 / ap as f64);
            assert!((reale / atteso - 1.0).abs() < 0.02, "{lp} × {ap} px per {l_mm} × {a_mm} mm");
        }
    }

    /// Il thread anteprime consegna comunque le immagini (con o senza pdfium):
    /// l'app resta utilizzabile anche se la libreria manca.
    #[test]
    fn il_thread_consegna_le_immagini() {
        let png = png_valido(60, 40);
        let (tx, rx) = std::sync::mpsc::channel();
        let (anteprime, _) = Anteprime::avvia(move |id, _, pagine| {
            let _ = tx.send((id, pagine.len()));
        });
        anteprime.richiedi(7, png.clone(), Formato::Png, Lavorazione::default(), None);
        let (id, n) = rx.recv_timeout(Duration::from_secs(10)).expect("consegna anteprima");
        std::fs::remove_file(&png).unwrap();
        assert_eq!((id, n), (7, 1));
    }

    /// L'anteprima di un PDF impaginato è il file che parte: pagina larga
    /// quanto il rotolo e contenuto nero dove deve stare. Il rendering di pdfium
    /// verifica anche il verso di /Rotate.
    #[test]
    fn pdf_impaginato_al_centro_e_a_destra() {
        let Some(pdfium) = pdfium_o_salta() else { return };
        let mm_pt = 72.0 / 25.4;
        // Rotolo da 400 mm, margine 10: un 100 × 50 mm orizzontale resta dritto,
        // un 50 × 100 verticale viene ruotato e diventa 100 × 50.
        for (l, a, ruotata) in [(100.0, 50.0, false), (50.0, 100.0, true)] {
            // Pagina da 380 mm (400 meno 10 per lato).
            for (allineamento, x_atteso) in [(Allineamento::Centro, 140.0), (Allineamento::Destra, 280.0)] {
                let imp = Impaginazione { allineamento, rotolo_mm: 400.0, margine_mm: 10.0 };
                let pdf = file_temp(".pdf");
                std::fs::write(&pdf, crate::impagina::test::pdf(l * mm_pt, a * mm_pt, (20.0, 40.0), 0)).unwrap();
                let pagine = pagine_pdf(&pdfium, &pdf, &Lavorazione::default(), Some(&imp)).unwrap();
                std::fs::remove_file(&pdf).unwrap();
                assert!(pagine[0].impaginata);
                assert_eq!(pagine[0].ruotata, ruotata);
                let (l_mm, a_mm) = pagine[0].mm.unwrap();
                assert!((l_mm - 380.0).abs() < 0.5 && (a_mm - 50.0).abs() < 0.5, "{l_mm} × {a_mm}");
                // Colonne nere della riga centrale, in mm dal bordo sinistro.
                let img = &pagine[0].immagine;
                let [lp, ap] = img.size;
                let px_mm = lp as f64 / 380.0;
                let nere: Vec<usize> = (0..lp).filter(|&x| img.pixels[ap / 2 * lp + x].r() < 128).collect();
                let (prima, ultima) = (nere[0] as f64 / px_mm, (*nere.last().unwrap() + 1) as f64 / px_mm);
                assert!((prima - x_atteso).abs() < 1.0, "{allineamento:?} {l}×{a}: inizia a {prima} mm");
                assert!((ultima - (x_atteso + 100.0)).abs() < 1.0, "{allineamento:?} {l}×{a}: finisce a {ultima} mm");
            }
        }
    }

    #[test]
    fn immagine_impaginata_larga_quanto_il_rotolo() {
        // 600 × 300 px senza DPI (300 presunti): 50,8 × 25,4 mm su rotolo da 200.
        let p = file_temp(".png");
        image::RgbImage::new(600, 300).save(&p).unwrap();
        let imp = Impaginazione { allineamento: Allineamento::Centro, rotolo_mm: 200.0, margine_mm: 5.0 };
        let pagina = immagine(&p, Formato::Png, &Lavorazione::default(), Some(&imp)).unwrap();
        std::fs::remove_file(&p).unwrap();
        let (l_mm, a_mm) = pagina.mm.unwrap();
        assert!((l_mm - 190.0).abs() < 0.5 && (a_mm - 25.4).abs() < 0.5, "{l_mm} × {a_mm}");
        let [lp, ap] = pagina.immagine.size;
        assert!(lp <= LATO_MAX as usize);
        let riga = &pagina.immagine.pixels[ap / 2 * lp..(ap / 2 + 1) * lp];
        let nere: Vec<usize> = (0..lp).filter(|&x| riga[x].r() < 128).collect();
        let centro = (nere[0] + nere.last().unwrap()) as f64 / 2.0 / lp as f64;
        assert!((centro - 0.5).abs() < 0.01, "centro a {centro}");
    }
}
