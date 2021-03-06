use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json formatting error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("number error: {0}")]
    Number(#[from] std::num::ParseIntError),
    #[error("utf8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("invalid varint")]
    Varint,
    #[error("packet too large")]
    PacketTooLarge,
}

/// Encode a u32 into a VarInt.
fn encode_varint(num: u32) -> Vec<u8> {
    let mut val = num;
    let mut varint: Vec<u8> = vec![];

    while val & 0xFFFF_FF80 != 0 {
        let item = (val & 0x7F) | 0x80;
        varint.push(item as u8);
        val >>= 7;
    }

    varint.push((val & 0x7F) as u8);

    varint
}

/// Read a VarInt into a u32 from an AsyncRead type.
async fn read_varint<T>(reader: &mut T) -> Result<u32, Error>
where
    T: AsyncRead + Unpin,
{
    // Storage for each byte as its read
    let mut buf: Vec<u8> = vec![0; 1];
    // Final result value
    let mut result: u32 = 0;
    // How many bits have been read
    let mut index = 0;

    loop {
        // Read a single byte
        reader.read_exact(&mut buf).await?;

        // Ignore top bit, only care about 7 bits right now
        // However, we need 32 bits of working space to shift
        let value = u32::from(buf[0] & 0b0111_1111);

        // Merge bits into previous bits after shifting to correct position
        result |= value << (7 * index);

        index += 1;
        // If length is greater than 5, something is wrong
        if index > 5 {
            return Err(Error::Varint);
        }

        // If top bit was zero, we're done
        if buf[0] & 0b1000_0000 == 0 {
            break;
        }
    }

    Ok(result)
}

/// Build a packet by:
/// * Encoding a representation of the ID into a VarInt
/// * Encoding the length of the ID and data into a VarInt
/// * Creating a Vec to store that metadata along with the data
fn build_packet(data: Vec<u8>, id: u32) -> Vec<u8> {
    let id = encode_varint(id);
    let len = encode_varint((data.len() + id.len()) as u32);

    // We know the exact size of the packet, so allocate exactly that.
    let mut packet = Vec::with_capacity(id.len() + len.len() + data.len());

    packet.extend(len);
    packet.extend(id);
    packet.extend(data);

    packet
}

/// Build a handshake packet by adding:
/// * Magic data
/// * Host length as a VarInt, the host, and the port
/// * Next state of status
fn build_handshake(host: &str, port: u16) -> Vec<u8> {
    // Default capacity calculated by expected values.
    // Explanation commented on each item as they are added.
    let mut data = Vec::with_capacity(5 + host.len());

    data.extend(encode_varint(0x47)); // 1 byte
    data.extend(encode_varint(host.len() as u32)); // probably 1 byte
    data.extend(host.as_bytes()); // `host.len()` bytes
    data.extend(&port.to_be_bytes()); // 2 bytes
    data.extend(encode_varint(1)); // 1 byte

    data
}

/// Server version info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub name: Option<String>,
    pub protocol: i32,
}

/// A player on the server and their ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerSample {
    pub name: String,
    pub id: String,
}

/// Info about players on a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Players {
    pub max: i32,
    pub online: i32,
    /// A subset of the players on the server.
    pub sample: Option<Vec<PlayerSample>>,
}

/// All info returned from a ping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ping {
    pub version: Version,
    pub players: Players,
    /// The description is arbitrary JSON data that may
    /// be parsed to get colors, etc.
    pub description: serde_json::Value,
    pub favicon: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotdExtra {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Motd {
    pub text: String,
    #[serde(default)]
    pub extra: Vec<MotdExtra>,
}

impl Ping {
    /// Extract all text fields from the server description.
    pub fn get_motd(&self) -> Option<String> {
        serde_json::from_value::<Motd>(self.description.clone())
            .ok()
            .map(|motd| {
                motd.text
                    .chars()
                    .chain(motd.extra.iter().flat_map(|extra| extra.text.chars()))
                    .collect()
            })
    }
}

/// Attempt to send a ping to a server.
///
/// Both server offline errors and resolution errors will be returned  as an
/// error. If the `bad_server` field in error is true it means that it is a
/// resolution or other failure. If it is false, the error was caused by not
/// being able to communicate with the server.
///
/// In order to avoid resource exhaustion it is advisable to wrap this in
/// a timeout as none are implemented within the library.
pub async fn send_ping(addr: SocketAddr, host: &str, port: u16) -> Result<Ping, Error> {
    // Resolve our host and port to a SocketAddr,
    // then open a TCP connection.
    let mut stream = TcpStream::connect(&addr).await?;

    // Create a handshake and write it.
    let handshake = build_packet(build_handshake(host, port), 0x00);
    stream.write_all(&handshake).await?;

    // Send a request packet.
    let request = build_packet(vec![], 0x00);
    stream.write_all(&request).await?;

    // Read the packet ID length and packet ID, discard values.
    // We do not care about what they were.
    let _packet_length = read_varint(&mut stream).await?;
    let _packet_id = read_varint(&mut stream).await?;

    // Read the data length and ensure it's of a reasonable size.
    let string_len = read_varint(&mut stream).await? as usize;
    if string_len > 1024 * 1024 * 10 {
        tracing::error!(
            "rejecting ping packet from {}:{}, desired size is {}",
            host,
            port,
            string_len
        );
        return Err(Error::PacketTooLarge);
    }

    // Attempt to allocate and read the packet.
    let mut data: Vec<u8> = vec![0; string_len];
    stream.read_exact(&mut data).await?;

    // Attempt to parse the data into a UTF8 string and deserialize its
    // JSON contents.
    let s = String::from_utf8(data)?;
    let ping: Ping = serde_json::from_str(&s)?;

    Ok(ping)
}

/// Parse plugins from an optional string.
fn parse_plugins(plugins: Option<String>) -> (String, Vec<String>) {
    // Ensure that we have plugins to parse. If not, return empty data.
    let plugins = match plugins {
        None => return ("".to_string(), vec![]),
        Some(plugins) => plugins,
    };

    // Plugin data is provided in a format like this:
    // `server_name: plugin1; plugin2`
    // Start by splitting off the server name.
    let mut parts = plugins.split(": ");

    // We always have a first part given that we had a string.
    let server_mod_name = parts.next().unwrap();

    // If we have another match, attempt to parse plugins.
    let plugins: Vec<String> = match parts.next() {
        Some(plugins) => plugins
            .split("; ")
            .map(|plugin| plugin.to_string())
            .collect(),
        None => vec![],
    };

    (server_mod_name.to_string(), plugins)
}

/// Info from a server query.
#[derive(Debug, Serialize)]
pub struct Query {
    pub kv: std::collections::HashMap<String, String>,
    pub server: (String, Vec<String>),
    pub players: Vec<String>,
}

/// Read data from an AsyncRead until a null byte is received, then convert data
/// into a string lossily.
///
/// If no data was received before a null byte, it returns none. If an error
/// occurs while reading, it discards the data and returns none.
async fn string_until_zero<T>(reader: &mut T) -> Option<String>
where
    T: AsyncRead + Unpin,
{
    let mut items: Vec<u8> = vec![];

    let mut buf = [0; 1];
    loop {
        reader.read_exact(&mut buf).await.as_ref().ok()?;

        match buf[0] {
            0x00 => break,
            _ => items.push(buf[0]),
        }
    }

    if items.is_empty() {
        return None;
    }

    Some(String::from_utf8_lossy(&items).to_string())
}

/// Extract a list of players from an AsyncRead.
///
/// `ignore_garbage` is used to ignore the padding bytes between previous data
/// and the list of players.
async fn parse_players<T>(mut reader: &mut T, ignore_garbage: bool) -> Vec<String>
where
    T: AsyncRead + Unpin,
{
    let mut players = vec![];

    // 10 bytes of padding to ignore if requested.
    if ignore_garbage {
        let mut _garbage = vec![0; 10];
        let _err = reader.read_exact(&mut _garbage).await;
    }

    // Keep reading strings until there's nothing left. Each string is a
    // player's username.
    while let Some(player) = string_until_zero(&mut reader).await {
        players.push(player);
    }

    players
}

/// Send a query to a server and get the response.
///
/// See [send_ping] for more information about timeouts and errors.
///
/// If data was missing, it is possible for fields to have empty values.
pub async fn send_query(addr: SocketAddr) -> Result<Query, Error> {
    // Resolve our host and port to a SocketAddr, bind a socket,
    // and open a UDP connection to the host.
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(addr).await?;

    // Generate and send a random session ID for our packet.
    let session_id = rand::random::<u32>() & 0x0F0F_0F0F;
    let mut request = vec![0xFE, 0xFD, 0x09];
    request.extend(&session_id.to_be_bytes());
    socket.send(&request).await?;

    // Receive up to 2KiB from connection.
    let mut buf: Vec<u8> = vec![0; 65_535];
    let len = socket.recv(&mut buf).await?;

    // Get the challenge token from the response.
    let challenge_token: i32 = String::from_utf8_lossy(&buf[5..len - 1]).parse()?;

    // Create a packet with our session ID and magic to generate a response.
    let mut request = vec![0xFE, 0xFD, 0x00];
    request.extend(&session_id.to_be_bytes());
    request.extend(&challenge_token.to_be_bytes());
    request.extend(vec![0x00, 0x00, 0x00, 0x00]);
    socket.send(&request).await?;

    // Receive data
    let len = socket.recv(&mut buf).await?;
    // Ignore type, session ID, and padding before trying to parse data.
    let mut cursor = std::io::Cursor::new(&buf[16..len - 1]);

    let mut kv = std::collections::HashMap::new();
    let mut server = None;

    while let Some(key) = string_until_zero(&mut cursor).await {
        let value = match string_until_zero(&mut cursor).await {
            Some(value) => value,
            _ => {
                tracing::warn!("had key {} with no value", key);
                continue;
            }
        };

        match key.as_ref() {
            "plugins" => {
                server = Some(parse_plugins(Some(value)));
            }
            _ => {
                kv.insert(key, value);
            }
        }
    }

    let players = parse_players(&mut cursor, true).await;

    Ok(Query {
        kv,
        players,
        server: server.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_varint() {
        assert_eq!(vec![0x00], encode_varint(0));
        assert_eq!(vec![0x01], encode_varint(1));
        assert_eq!(vec![0xFF, 0x01], encode_varint(255));
        assert_eq!(
            vec![0xFF, 0xFF, 0xFF, 0xFF, 0x07],
            encode_varint(2_147_483_647)
        );
    }

    #[tokio::test]
    async fn test_read_varint() {
        let src: Vec<u8> = vec![0x00];
        assert_eq!(0, read_varint(&mut src.as_slice()).await.unwrap());

        let src: Vec<u8> = vec![0x01];
        assert_eq!(1, read_varint(&mut src.as_slice()).await.unwrap());

        let src: Vec<u8> = vec![0xFF, 0x01];
        assert_eq!(255, read_varint(&mut src.as_slice()).await.unwrap());

        let src: Vec<u8> = vec![0b1000_0100, 0b0100_0000];
        assert_eq!(8196, read_varint(&mut src.as_slice()).await.unwrap());

        let src: Vec<u8> = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x07];
        assert_eq!(
            2_147_483_647,
            read_varint(&mut src.as_slice()).await.unwrap()
        );
    }

    #[test]
    fn test_build_packet() {
        let packet = build_packet(vec![], 0x00);
        assert_eq!(packet, vec![0x01, 0x00]);

        let packet = build_packet(vec![0x00], 0x00);
        assert_eq!(packet, vec![0x02, 0x00, 0x00]);
    }

    #[tokio::test]
    async fn test_string_until_zero() {
        let mut cursor = std::io::Cursor::new(vec![102, 111, 120, 0, 104, 105, 0]);

        let msg = string_until_zero(&mut cursor).await;

        assert!(!msg.is_none());
        assert_eq!(msg.unwrap(), "fox");

        let msg = string_until_zero(&mut cursor).await;

        assert!(!msg.is_none());
        assert_eq!(msg.unwrap(), "hi");

        let msg = string_until_zero(&mut cursor).await;

        assert!(msg.is_none());
    }

    #[test]
    fn test_parse_plugins() {
        let plugins = parse_plugins(None);
        assert_eq!(plugins.0, "");
        assert_eq!(plugins.1.len(), 0);

        let plugins = parse_plugins(Some("CraftBukkit on Bukkit 1.2.5-R4.0".to_string()));
        assert_eq!(plugins.0, "CraftBukkit on Bukkit 1.2.5-R4.0");
        assert_eq!(plugins.1.len(), 0);

        let plugins = parse_plugins(Some(
            "CraftBukkit on Bukkit 1.2.5-R4.0: WorldEdit 5.3; CommandBook 2.1".to_string(),
        ));
        assert_eq!(plugins.0, "CraftBukkit on Bukkit 1.2.5-R4.0");
        assert_eq!(plugins.1, vec!["WorldEdit 5.3", "CommandBook 2.1"]);
    }

    #[tokio::test]
    async fn test_parse_players() {
        let mut cursor = std::io::Cursor::new(vec![97, 0, 98, 0, 99, 0, 0]);
        let players = parse_players(&mut cursor, false).await;

        assert_eq!(players, vec!["a", "b", "c"]);
    }
}
