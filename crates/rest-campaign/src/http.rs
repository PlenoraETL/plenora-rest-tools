//! HTTP/1.1 minimo lato server, scritto a mano come nei test black-box del
//! motore: abbastanza per controllare byte per byte che cosa riceve il
//! servizio remoto (corpo, header, connessioni) e per iniettare guasti a un
//! punto preciso dello scambio.
//!
//! Il parser rifiuta tutto ciò che non capisce (una richiesta TLS verso la
//! porta in chiaro, una testata oltre il limite, un chunk malformato): la
//! connessione viene chiusa e contata come malformata, mai interpretata.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

const MAX_HEAD_BYTES: usize = 64 * 1024;
const READ_CHUNK: usize = 16 * 1024;

/// Richiesta ricevuta. Il corpo non viene trattenuto: se ne contano i byte e
/// se ne calcola lo SHA-256, così anche un upload grande occupa memoria
/// costante.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    /// Nomi in minuscolo, nell'ordine di arrivo.
    pub headers: Vec<(String, String)>,
    pub body_bytes: u64,
    pub body_sha256: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn wants_close(&self) -> bool {
        self.header("connection")
            .is_some_and(|value| value.eq_ignore_ascii_case("close"))
    }
}

/// Esito della lettura di una richiesta.
pub enum ReadOutcome {
    Request(Request),
    /// Il client ha chiuso prima di iniziare una nuova richiesta.
    Closed,
    /// Byte che non formano una richiesta HTTP/1.1 valida.
    Malformed,
}

/// Connessione con il buffer dei byte già letti e non ancora consumati.
pub struct Connection {
    pub stream: TcpStream,
    buffer: Vec<u8>,
}

impl Connection {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    async fn fill(&mut self) -> std::io::Result<usize> {
        let mut chunk = vec![0_u8; READ_CHUNK];
        let read = self.stream.read(&mut chunk).await?;
        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(read)
    }

    /// Legge testata e corpo della prossima richiesta.
    pub async fn read_request(&mut self) -> ReadOutcome {
        let head_end = loop {
            // Un metodo HTTP inizia con una lettera maiuscola: un ClientHello
            // TLS (0x16) si rifiuta subito, senza attendere byte che il
            // client non manderà finché non riceve la risposta TLS.
            if self
                .buffer
                .first()
                .is_some_and(|byte| !byte.is_ascii_uppercase())
            {
                return ReadOutcome::Malformed;
            }
            if let Some(position) = find(&self.buffer, b"\r\n\r\n") {
                break position;
            }
            if self.buffer.len() > MAX_HEAD_BYTES {
                return ReadOutcome::Malformed;
            }
            match self.fill().await {
                Ok(0) if self.buffer.is_empty() => return ReadOutcome::Closed,
                Ok(0) | Err(_) => return ReadOutcome::Malformed,
                Ok(_) => {}
            }
        };
        let head: Vec<u8> = self.buffer.drain(..head_end + 4).collect();
        let Ok(head) = String::from_utf8(head) else {
            return ReadOutcome::Malformed;
        };
        let mut lines = head.split("\r\n");
        let Some(request_line) = lines.next() else {
            return ReadOutcome::Malformed;
        };
        let mut parts = request_line.split(' ');
        let (Some(method), Some(target), Some(version), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return ReadOutcome::Malformed;
        };
        if version != "HTTP/1.1" && version != "HTTP/1.0" {
            return ReadOutcome::Malformed;
        }
        let mut headers = Vec::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let Some((name, value)) = line.split_once(':') else {
                return ReadOutcome::Malformed;
            };
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
        let (path, query) = split_target(target);
        let mut request = Request {
            method: method.to_owned(),
            path,
            query,
            headers,
            body_bytes: 0,
            body_sha256: String::new(),
        };
        let chunked = request
            .header("transfer-encoding")
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
        let mut digest = Sha256::new();
        let body = if chunked {
            self.read_chunked(&mut digest).await
        } else {
            match request.header("content-length") {
                None => Some(0),
                Some(value) => match value.parse::<u64>() {
                    Ok(length) => self.read_exact_body(length, &mut digest).await,
                    Err(_) => None,
                },
            }
        };
        let Some(body_bytes) = body else {
            return ReadOutcome::Malformed;
        };
        request.body_bytes = body_bytes;
        request.body_sha256 = hex(&digest.finalize());
        ReadOutcome::Request(request)
    }

    async fn read_exact_body(&mut self, length: u64, digest: &mut Sha256) -> Option<u64> {
        let mut remaining = length;
        while remaining > 0 {
            if self.buffer.is_empty() {
                match self.fill().await {
                    Ok(0) | Err(_) => return None,
                    Ok(_) => {}
                }
            }
            let take = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(self.buffer.len());
            digest.update(&self.buffer[..take]);
            self.buffer.drain(..take);
            remaining -= take as u64;
        }
        Some(length)
    }

    async fn read_line(&mut self) -> Option<String> {
        loop {
            if let Some(position) = find(&self.buffer, b"\r\n") {
                let line: Vec<u8> = self.buffer.drain(..position + 2).collect();
                return String::from_utf8(line[..position].to_vec()).ok();
            }
            if self.buffer.len() > MAX_HEAD_BYTES {
                return None;
            }
            match self.fill().await {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn read_chunked(&mut self, digest: &mut Sha256) -> Option<u64> {
        let mut total = 0_u64;
        loop {
            let line = self.read_line().await?;
            let size_text = line.split(';').next()?.trim();
            let size = u64::from_str_radix(size_text, 16).ok()?;
            if size == 0 {
                // Trailer opzionali fino alla riga vuota.
                loop {
                    let trailer = self.read_line().await?;
                    if trailer.is_empty() {
                        return Some(total);
                    }
                }
            }
            self.read_exact_body(size, digest).await?;
            total = total.checked_add(size)?;
            let terminator = self.read_line().await?;
            if !terminator.is_empty() {
                return None;
            }
        }
    }

    /// Attende che il client chiuda la connessione (o che arrivino byte, che
    /// vengono scartati) fino a `limit`.
    pub async fn wait_for_close(&mut self, limit: std::time::Duration) {
        let _ = tokio::time::timeout(limit, async {
            let mut sink = vec![0_u8; READ_CHUNK];
            loop {
                match self.stream.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await;
    }
}

/// Risposta completa con `Content-Length`.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.to_string().into_bytes(),
        }
    }

    pub fn with_header(mut self, name: &str, value: String) -> Self {
        self.headers.push((name.to_owned(), value));
        self
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        206 => "Partial Content",
        401 => "Unauthorized",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Testata di risposta; `content_length` è quella dichiarata, che un guasto
/// di troncamento può non rispettare.
pub fn response_head(
    status: u16,
    headers: &[(String, String)],
    content_length: u64,
    keep_alive: bool,
) -> String {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str(&format!("Content-Length: {content_length}\r\n"));
    if !keep_alive {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    head
}

pub async fn write_response(
    stream: &mut TcpStream,
    response: &Response,
    keep_alive: bool,
) -> std::io::Result<()> {
    let head = response_head(
        response.status,
        &response.headers,
        response.body.len() as u64,
        keep_alive,
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.flush().await
}

fn split_target(target: &str) -> (String, BTreeMap<String, String>) {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (name.to_owned(), value.to_owned()),
            None => (pair.to_owned(), String::new()),
        })
        .collect();
    (path.to_owned(), query)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{ReadOutcome, split_target};
    use tokio::{
        io::AsyncWriteExt,
        net::{TcpListener, TcpStream},
    };

    #[test]
    fn targets_split_into_path_and_query() {
        let (path, query) = split_target("/pages/k/offset?offset=10&limit=5&flag");
        assert_eq!(path, "/pages/k/offset");
        assert_eq!(query.get("offset").map(String::as_str), Some("10"));
        assert_eq!(query.get("flag").map(String::as_str), Some(""));
    }

    async fn parse(raw: &'static [u8]) -> ReadOutcome {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(raw).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let (stream, _) = listener.accept().await.unwrap();
        let mut connection = super::Connection::new(stream);
        let outcome = connection.read_request().await;
        client.await.unwrap();
        outcome
    }

    #[tokio::test]
    async fn chunked_and_sized_bodies_are_hashed_and_tls_is_refused() {
        let ReadOutcome::Request(sized) =
            parse(b"PUT /upload/k HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc").await
        else {
            panic!("richiesta con Content-Length non letta");
        };
        assert_eq!(sized.body_bytes, 3);
        assert_eq!(
            sized.body_sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let ReadOutcome::Request(chunked) = parse(
            b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n2\r\nbc\r\n0\r\n\r\n",
        )
        .await
        else {
            panic!("richiesta chunked non letta");
        };
        assert_eq!(chunked.body_bytes, 3);
        assert_eq!(chunked.body_sha256, sized.body_sha256);
        // Un ClientHello TLS non è HTTP.
        assert!(matches!(
            parse(b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03").await,
            ReadOutcome::Malformed
        ));
    }
}
