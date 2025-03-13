use std::{
    net::SocketAddr,
    str::FromStr,
    time::{Duration, Instant},
};

use anyhow::Context;
use async_io::Timer;
use clap::{Parser, Subcommand};

use futures_util::io::{AsyncReadExt as _, AsyncWriteExt as _};
use rand::Rng;
use sillad::{
    dialer::{Dialer, DialerExt},
    listener::{Listener, ListenerExt},
    Pipe,
};
use sillad_native_tls::{TlsDialer, TlsListener};
use sillad_sosistab3::{dialer::SosistabDialer, listener::SosistabListener, Cookie};
use smolscale::spawn;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a server for obfuscation testing
    Server {
        /// Address to listen on (e.g. "0.0.0.0:12345")
        #[arg(long)]
        listen: String,

        /// Obfuscation protocol to use (sosistab3 or tls)
        #[arg(long)]
        protocol: String,

        /// Cookie to use for sosistab3 protocol
        #[arg(long)]
        cookie: Option<String>,
    },

    /// Run a client to connect to the obfuscation test server
    Client {
        /// Server to connect to (e.g. "127.0.0.1:12345")
        #[arg(long)]
        connect: String,

        /// Obfuscation protocol to use (sosistab3 or tls)
        #[arg(long)]
        protocol: String,

        /// Cookie to use for sosistab3 protocol
        #[arg(long)]
        cookie: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Parse command line arguments
    let cli = Cli::parse();

    // Dispatch to appropriate subcommand
    match cli.command {
        Commands::Server {
            listen,
            protocol,
            cookie,
        } => {
            let addr = SocketAddr::from_str(&listen)?;
            smolscale::block_on(server(addr, protocol, cookie))?;
        }
        Commands::Client {
            connect,
            protocol,
            cookie,
        } => {
            let addr = SocketAddr::from_str(&connect)?;
            smolscale::block_on(client(addr, protocol, cookie))?;
        }
    }

    Ok(())
}

async fn server(
    listen_addr: SocketAddr,
    protocol: String,
    cookie: Option<String>,
) -> anyhow::Result<()> {
    // Create a TCP listener
    let tcp_listener = sillad::tcp::TcpListener::bind(listen_addr).await?;
    tracing::info!("Listening on {}", listen_addr);

    let listener = match protocol.as_str() {
        "tls" => {
            // Create a TLS listener
            let tls_config = create_tls_acceptor();
            let tls_listener = TlsListener::new(tcp_listener, tls_config);
            tls_listener.dynamic()
        }
        "sosistab3" => {
            // Create a Sosistab3 listener
            let cookie_str = cookie.context("sosistab3 requires a cookie parameter")?;
            let cookie = Cookie::new(&cookie_str);
            let sosistab_listener = SosistabListener::new(tcp_listener, cookie);
            sosistab_listener.dynamic()
        }
        _ => {
            return Err(anyhow::anyhow!("Unsupported protocol: {}", protocol));
        }
    };

    // Accept connections in a loop
    let mut listener = listener;
    loop {
        let stream = listener.accept().await?;
        tracing::info!("New connection accepted");

        // Spawn a new task to handle the connection
        spawn(handle_connection(stream)).detach();
    }
}

async fn client(
    connect_addr: SocketAddr,
    protocol: String,
    cookie: Option<String>,
) -> anyhow::Result<()> {
    tracing::info!("Connecting to {}", connect_addr);

    let tcp_dialer = sillad::tcp::TcpDialer {
        dest_addr: connect_addr,
    };

    let dialer = match protocol.as_str() {
        "tls" => {
            // Create a TLS dialer
            let connector = async_native_tls::TlsConnector::new().danger_accept_invalid_certs(true);
            let tls_dialer = TlsDialer::new(tcp_dialer, connector, "example.com".to_string());
            tls_dialer.dynamic()
        }
        "sosistab3" => {
            // Create a Sosistab3 dialer
            let cookie_str = cookie.context("sosistab3 requires a cookie parameter")?;
            let cookie = Cookie::new(&cookie_str);
            let sosistab_dialer = SosistabDialer {
                inner: tcp_dialer,
                cookie,
            };
            sosistab_dialer.dynamic()
        }
        _ => {
            return Err(anyhow::anyhow!("Unsupported protocol: {}", protocol));
        }
    };

    // Connect to the server
    let stream = dialer.dial().await?;
    tracing::info!("Connected to server");

    // Handle the client side of communication
    handle_client(stream).await?;

    Ok(())
}

async fn handle_connection(mut stream: impl Pipe) -> anyhow::Result<()> {
    let mut buffer = [0u8; 4096];

    // Echo back data
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => {
                // Connection closed
                tracing::info!("Connection closed by client");
                break;
            }
            Ok(n) => {
                // Echo the data back
                stream.write_all(&buffer[..n]).await?;
                stream.flush().await?;
            }
            Err(e) => {
                tracing::error!("Error reading from client: {}", e);
                break;
            }
        }
    }

    Ok(())
}

async fn handle_client(stream: impl Pipe) -> anyhow::Result<()> {
    // Start ping measurement
    measure_ping(stream).await?;

    Ok(())
}

async fn measure_ping(stream: impl Pipe) -> anyhow::Result<()> {
    // Use the futures_util split utility to split the stream into readable and writable halves
    let (mut read_stream, mut write_stream) = stream.split();

    // Buffer for reading responses
    let mut buffer = [0u8; 4096];

    println!("Starting ping measurements...");
    println!("Press Ctrl+C to exit");

    loop {
        // Generate a random message length
        let message_len = rand::thread_rng().gen_range(10..2000);
        let mut message = vec![0u8; message_len];
        rand::thread_rng().fill(&mut message[..]);

        // Append a newline to mark the end of the message
        message.push(b'\n');

        // Send the message and record the time
        let start = Instant::now();
        write_stream.write_all(&message).await?;
        write_stream.flush().await?;

        // Read the response
        let mut total_read = 0;
        while total_read < message.len() {
            let bytes_read = read_stream
                .read(&mut buffer[total_read..message.len()])
                .await?;
            if bytes_read == 0 {
                return Err(anyhow::anyhow!("Connection closed by server"));
            }
            total_read += bytes_read;
        }

        // Calculate and display the ping
        let ping = start.elapsed();
        println!("Ping: {:?} | Message size: {} bytes", ping, message.len());

        // Wait a bit before the next ping
        Timer::after(Duration::from_millis(500)).await;
    }
}

// Create a TLS acceptor for the server
fn create_tls_acceptor() -> async_native_tls::TlsAcceptor {
    // Generate random subject alt names for the certificate
    let subject_alt_names = (0..5)
        .map(|_| format!("{}.example.com", rand::random::<u16>()))
        .collect::<Vec<_>>();

    // Create certificate parameters
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "example.com");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "Geph Testing");
    params.subject_alt_names = subject_alt_names
        .iter()
        .map(|san| rcgen::SanType::DnsName(san.clone().try_into().unwrap()))
        .collect();

    // Set certificate validity period
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(365);

    // Generate the key pair and certificate
    let keypair = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&keypair).unwrap();

    // Convert to PEM format
    let cert_pem = cert.pem();
    let key_pem = keypair.serialize_pem();

    // Create the TLS identity
    let identity = native_tls::Identity::from_pkcs8(cert_pem.as_bytes(), key_pem.as_bytes())
        .expect("Failed to create TLS identity");

    // Build the TLS acceptor
    let mut builder = native_tls::TlsAcceptor::builder(identity);
    builder.min_protocol_version(Some(native_tls::Protocol::Tlsv10));
    builder.max_protocol_version(Some(native_tls::Protocol::Tlsv12));

    builder.build().unwrap().into()
}
