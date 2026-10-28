//! Client SNMP v1 minimo (GET e GETNEXT) per leggere lo stato della stampante.
//!
//! La T2600 a `@PJL INFO STATUS` risponde solo `"?"`: lo stato vero si legge
//! dalle MIB standard (HOST-RESOURCES-MIB, Printer-MIB RFC 3805), come fa HP Click.

use std::net::{ToSocketAddrs, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug, PartialEq)]
pub enum Valore {
    Intero(i64),
    Testo(Vec<u8>),
    Oid(Vec<u32>),
    Nullo,
    Altro(u8, Vec<u8>),
}

impl Valore {
    pub fn intero(&self) -> Option<i64> {
        match self {
            Valore::Intero(i) => Some(*i),
            _ => None,
        }
    }

    pub fn testo(&self) -> Option<String> {
        match self {
            Valore::Testo(b) => Some(String::from_utf8_lossy(b).trim_end_matches('\0').trim().to_string()),
            _ => None,
        }
    }
}

pub fn oid(s: &str) -> Vec<u32> {
    s.trim_start_matches('.').split('.').filter_map(|p| p.parse().ok()).collect()
}

// --- Codifica BER ---------------------------------------------------------

fn tlv(tag: u8, contenuto: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = contenuto.len();
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let byte: Vec<u8> = n.to_be_bytes().into_iter().skip_while(|&b| b == 0).collect();
        out.push(0x80 | byte.len() as u8);
        out.extend(byte);
    }
    out.extend_from_slice(contenuto);
    out
}

fn intero(v: i64) -> Vec<u8> {
    let b = v.to_be_bytes();
    // Toglie i byte iniziali ridondanti mantenendo il segno.
    let mut i = 0;
    while i < 7 && ((b[i] == 0 && b[i + 1] & 0x80 == 0) || (b[i] == 0xFF && b[i + 1] & 0x80 != 0)) {
        i += 1;
    }
    tlv(0x02, &b[i..])
}

fn codifica_oid(oid: &[u32]) -> Vec<u8> {
    let mut c = vec![(oid[0] * 40 + oid[1]) as u8];
    for &arco in &oid[2..] {
        let mut gruppi = vec![(arco & 0x7F) as u8];
        let mut v = arco >> 7;
        while v > 0 {
            gruppi.push((v & 0x7F) as u8 | 0x80);
            v >>= 7;
        }
        c.extend(gruppi.into_iter().rev());
    }
    tlv(0x06, &c)
}

fn richiesta(comunita: &str, id: i32, getnext: bool, oids: &[Vec<u32>]) -> Vec<u8> {
    let varbind: Vec<u8> = oids
        .iter()
        .flat_map(|o| tlv(0x30, &[codifica_oid(o), vec![0x05, 0x00]].concat()))
        .collect();
    let pdu = [intero(id.into()), intero(0), intero(0), tlv(0x30, &varbind)].concat();
    let pdu = tlv(if getnext { 0xA1 } else { 0xA0 }, &pdu);
    tlv(0x30, &[intero(0), tlv(0x04, comunita.as_bytes()), pdu].concat())
}

// --- Decodifica BER -------------------------------------------------------

struct Lettore<'a>(&'a [u8]);

impl<'a> Lettore<'a> {
    fn leggi(&mut self) -> Result<(u8, &'a [u8])> {
        let d = self.0;
        if d.len() < 2 {
            bail!("risposta SNMP troncata");
        }
        let tag = d[0];
        let (lung, inizio) = if d[1] & 0x80 == 0 {
            (d[1] as usize, 2)
        } else {
            let n = (d[1] & 0x7F) as usize;
            if n == 0 || n > 4 || d.len() < 2 + n {
                bail!("lunghezza BER non valida");
            }
            (d[2..2 + n].iter().fold(0usize, |a, &b| a << 8 | b as usize), 2 + n)
        };
        if d.len() < inizio + lung {
            bail!("risposta SNMP troncata");
        }
        self.0 = &d[inizio + lung..];
        Ok((tag, &d[inizio..inizio + lung]))
    }

    fn atteso(&mut self, tag: u8) -> Result<&'a [u8]> {
        let (t, c) = self.leggi()?;
        if t != tag {
            bail!("tag BER inatteso: {t:#04x} invece di {tag:#04x}");
        }
        Ok(c)
    }
}

fn decodifica_intero(c: &[u8]) -> i64 {
    let iniziale = if c.first().is_some_and(|b| b & 0x80 != 0) { -1 } else { 0 };
    c.iter().fold(iniziale, |a, &b| a << 8 | b as i64)
}

fn decodifica_oid(c: &[u8]) -> Vec<u32> {
    let Some((&primo, resto)) = c.split_first() else { return Vec::new() };
    let mut out = vec![(primo / 40) as u32, (primo % 40) as u32];
    let mut v = 0u32;
    for &b in resto {
        v = v << 7 | (b & 0x7F) as u32;
        if b & 0x80 == 0 {
            out.push(v);
            v = 0;
        }
    }
    out
}

fn decodifica_risposta(dati: &[u8], id_atteso: i32) -> Result<Vec<(Vec<u32>, Valore)>> {
    let mut msg = Lettore(Lettore(dati).atteso(0x30)?);
    msg.atteso(0x02)?; // versione
    msg.atteso(0x04)?; // comunità
    let mut pdu = Lettore(msg.atteso(0xA2)?);
    if decodifica_intero(pdu.atteso(0x02)?) != id_atteso as i64 {
        bail!("risposta SNMP a un'altra richiesta");
    }
    let errore = decodifica_intero(pdu.atteso(0x02)?);
    pdu.atteso(0x02)?;
    let mut lista = Lettore(pdu.atteso(0x30)?);
    let mut out = Vec::new();
    // noSuchName (2) in v1 vuol dire "fine della tabella" per GETNEXT: lista vuota.
    if errore == 2 {
        return Ok(out);
    }
    if errore != 0 {
        bail!("errore SNMP {errore}");
    }
    while !lista.0.is_empty() {
        let mut vb = Lettore(lista.atteso(0x30)?);
        let o = decodifica_oid(vb.atteso(0x06)?);
        let (tag, c) = vb.leggi()?;
        let v = match tag {
            0x02 | 0x41 | 0x42 | 0x43 => Valore::Intero(decodifica_intero(c)),
            0x04 => Valore::Testo(c.to_vec()),
            0x06 => Valore::Oid(decodifica_oid(c)),
            0x05 => Valore::Nullo,
            t => Valore::Altro(t, c.to_vec()),
        };
        out.push((o, v));
    }
    Ok(out)
}

// --- Trasporto ------------------------------------------------------------

pub struct Client {
    sock: UdpSocket,
    comunita: String,
    prossimo_id: i32,
}

impl Client {
    pub fn new(host: &str, comunita: &str) -> Result<Self> {
        Self::new_con_porta(host, 161, comunita)
    }

    pub fn new_con_porta(host: &str, porta: u16, comunita: &str) -> Result<Self> {
        let indirizzo = (host, porta)
            .to_socket_addrs()?
            .next()
            .with_context(|| format!("nessun indirizzo per {host}"))?;
        let sock = UdpSocket::bind(if indirizzo.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
        sock.connect(indirizzo)?;
        sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        Ok(Self { sock, comunita: comunita.into(), prossimo_id: std::process::id() as i32 & 0x7FFF })
    }

    fn scambia(&mut self, getnext: bool, oids: &[Vec<u32>]) -> Result<Vec<(Vec<u32>, Valore)>> {
        self.prossimo_id = self.prossimo_id.wrapping_add(1) & 0x7FFF_FFFF;
        let id = self.prossimo_id;
        let pacchetto = richiesta(&self.comunita, id, getnext, oids);
        let mut buf = vec![0u8; 65535];
        for _ in 0..2 {
            self.sock.send(&pacchetto)?;
            while let Ok(n) = self.sock.recv(&mut buf) {
                // Una risposta in ritardo a una richiesta precedente non si decodifica: si scarta.
                if let Ok(r) = decodifica_risposta(&buf[..n], id) {
                    return Ok(r);
                }
            }
        }
        bail!("nessuna risposta SNMP (stampante spenta o SNMP disattivato?)")
    }

    pub fn get(&mut self, oids: &[Vec<u32>]) -> Result<Vec<(Vec<u32>, Valore)>> {
        self.scambia(false, oids)
    }

    /// Tutte le voci sotto `base`, con GETNEXT successivi.
    pub fn walk(&mut self, base: &[u32]) -> Result<Vec<(Vec<u32>, Valore)>> {
        let mut out = Vec::new();
        let mut corrente = base.to_vec();
        while out.len() < 500 {
            let Some((o, v)) = self.scambia(true, std::slice::from_ref(&corrente))?.into_iter().next() else {
                break;
            };
            if !o.starts_with(base) || o <= corrente {
                break;
            }
            corrente = o.clone();
            out.push((o, v));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn oid_andata_e_ritorno() {
        let o = oid("1.3.6.1.2.1.43.11.1.1.9.1.300");
        let c = codifica_oid(&o);
        assert_eq!(decodifica_oid(&c[2..]), o);
    }

    #[test]
    fn interi_con_segno() {
        for v in [0, 1, 127, 128, 255, 256, -1, -2, -129, 1 << 40] {
            let c = intero(v);
            assert_eq!(decodifica_intero(&c[2..]), v, "{v}");
        }
        assert_eq!(intero(128), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(intero(-2), vec![0x02, 0x01, 0xFE]);
    }

    #[test]
    fn richiesta_get_come_net_snmp() {
        // snmpget -v1 -c public HOST 1.3.6.1.2.1.1.1.0 con request-id 1
        let atteso = [
            0x30, 0x26, 0x02, 0x01, 0x00, 0x04, 0x06, b'p', b'u', b'b', b'l', b'i', b'c', 0xA0, 0x19,
            0x02, 0x01, 0x01, 0x02, 0x01, 0x00, 0x02, 0x01, 0x00, 0x30, 0x0E, 0x30, 0x0C, 0x06, 0x08,
            0x2B, 0x06, 0x01, 0x02, 0x01, 0x01, 0x01, 0x00, 0x05, 0x00,
        ];
        assert_eq!(richiesta("public", 1, false, &[oid("1.3.6.1.2.1.1.1.0")]), atteso);
    }

    #[test]
    fn decodifica_get_response() {
        let mut r = richiesta("public", 7, false, &[oid("1.3.6.1.2.1.25.3.5.1.1.1")]);
        // La trasformo in una GetResponse con valore INTEGER 3 (idle).
        let pos = r.iter().position(|&b| b == 0xA0).unwrap();
        r[pos] = 0xA2;
        let n = r.len();
        r[n - 2..].copy_from_slice(&[0x02, 0x01]);
        r.push(0x03);
        // Aggiorna le lunghezze (tutte in forma corta).
        for (i, delta) in [(1usize, 1u8), (pos + 1, 1)] {
            r[i] += delta;
        }
        let vb_seq = r.iter().rposition(|&b| b == 0x30).unwrap();
        r[vb_seq + 1] += 1;
        r[vb_seq - 1] += 1;
        let v = decodifica_risposta(&r, 7).unwrap();
        assert_eq!(v, vec![(oid("1.3.6.1.2.1.25.3.5.1.1.1"), Valore::Intero(3))]);
    }
}
