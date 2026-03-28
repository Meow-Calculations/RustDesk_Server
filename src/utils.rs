use dns_lookup::{lookup_addr, lookup_host};
use hbb_common::{anyhow, bail, ResultType};
use sodiumoxide::crypto::sign;
use std::{
    env,
    net::{IpAddr, TcpStream},
    process, str,
};

fn print_help() {
    println!(
        "Usage:
    rustdesk-utils [command]\n
Available Commands:
    genkeypair                                   Generate a new keypair
    validatekeypair [public key] [secret key]    Validate an existing keypair
    doctor [rustdesk-server]                     Check for server connection problems"
    );
    process::exit(0x0001);
}

fn error_then_help(msg: &str) {
    println!("ERROR: {msg}\n");
    print_help();
}

fn gen_keypair() {
    let (pk, sk) = sign::gen_keypair();
    let public_key = base64::encode(pk);
    let secret_key = base64::encode(sk);
    println!("Public Key:  {public_key}");
    println!("Secret Key:  {secret_key}");
}

// 安全修复 L-04: 重构为惯用 Rust 的 ? 操作符模式，消除冗余 unwrap
fn validate_keypair(pk: &str, sk: &str) -> ResultType<()> {
    let sk1 = base64::decode(sk).map_err(|_| anyhow::anyhow!("无效的密钥 (Secret Key)"))?;
    let secret_key = sign::SecretKey::from_slice(sk1.as_slice())
        .ok_or_else(|| anyhow::anyhow!("无效的密钥 (Secret Key)"))?;

    let pk1 = base64::decode(pk).map_err(|_| anyhow::anyhow!("无效的公钥 (Public Key)"))?;
    let public_key = sign::PublicKey::from_slice(pk1.as_slice())
        .ok_or_else(|| anyhow::anyhow!("无效的公钥 (Public Key)"))?;

    let random_data_to_test = b"This is meh.";
    let signed_data = sign::sign(random_data_to_test, &secret_key);
    let verified_data = sign::verify(&signed_data, &public_key)
        .map_err(|_| anyhow::anyhow!("密钥对验证失败 (Key pair is INVALID)"))?;

    if random_data_to_test != &verified_data[..] {
        bail!("密钥对验证失败 (Key pair is INVALID)");
    }

    Ok(())
}

fn doctor_tcp(address: std::net::IpAddr, port: &str, desc: &str) {
    let start = std::time::Instant::now();
    let conn = format!("{address}:{port}");
    if let Ok(_stream) = TcpStream::connect(conn.as_str()) {
        let elapsed = std::time::Instant::now().duration_since(start);
        println!(
            "TCP Port {} ({}): OK in {} ms",
            port,
            desc,
            elapsed.as_millis()
        );
    } else {
        println!("TCP Port {port} ({desc}): ERROR");
    }
}

fn doctor_ip(server_ip_address: std::net::IpAddr, server_address: Option<&str>) {
    println!("\nChecking IP address: {server_ip_address}");
    println!("Is IPV4: {}", server_ip_address.is_ipv4());
    println!("Is IPV6: {}", server_ip_address.is_ipv6());

    // 安全修复 H-09: DNS 反向查找可能失败，使用优雅错误处理替代 unwrap
    let reverse = match lookup_addr(&server_ip_address) {
        Ok(addr) => addr,
        Err(e) => {
            println!("反向 DNS 查找失败: {e}");
            "<unknown>".to_string()
        }
    };
    if let Some(server_address) = server_address {
        if reverse == server_address {
            println!("Reverse DNS lookup: '{reverse}' MATCHES server address");
        } else {
            println!(
                "Reverse DNS lookup: '{reverse}' DOESN'T MATCH server address '{server_address}'"
            );
        }
    }

    // TODO: ICMP ping 检测?

    // 端口检查 TCP（UDP 难以检查）
    doctor_tcp(server_ip_address, "21114", "API");
    doctor_tcp(server_ip_address, "21115", "hbbs NAT 检测额外端口");
    doctor_tcp(server_ip_address, "21116", "hbbs");
    doctor_tcp(server_ip_address, "21117", "hbbr tcp");
    doctor_tcp(server_ip_address, "21118", "hbbs websocket");
    doctor_tcp(server_ip_address, "21119", "hbbr websocket");

    // TODO: 密钥检查
}

fn doctor(server_address_unclean: &str) {
    let server_address3 = server_address_unclean.trim();
    let server_address2 = server_address3.to_lowercase();
    let server_address = server_address2.as_str();
    println!("Checking server:  {server_address}\n");
    if let Ok(server_ipaddr) = server_address.parse::<IpAddr>() {
        // 用户请求的是 IP 地址
        doctor_ip(server_ipaddr, None);
    } else {
        // 传入的字符串不是 IP 地址
        // 安全修复 H-09: DNS 正向查找可能失败，使用优雅错误处理替代 unwrap
        let ips: Vec<std::net::IpAddr> = match lookup_host(server_address) {
            Ok(ips) => ips,
            Err(e) => {
                println!("DNS 查找失败: {e}，请检查域名是否正确");
                return;
            }
        };
        println!("Found {} IP addresses: ", ips.len());

        ips.iter().for_each(|ip| println!(" - {ip}"));

        ips.iter()
            .for_each(|ip| doctor_ip(*ip, Some(server_address)));
    }
}

fn main() {
    let args: Vec<_> = env::args().collect();
    if args.len() <= 1 {
        print_help();
    }

    let command = args[1].to_lowercase();
    match command.as_str() {
        "genkeypair" => gen_keypair(),
        "validatekeypair" => {
            if args.len() <= 3 {
                error_then_help("You must supply both the public and the secret key");
            }
            let res = validate_keypair(args[2].as_str(), args[3].as_str());
            if let Err(e) = res {
                println!("{e}");
                process::exit(0x0001);
            }
            println!("Key pair is VALID");
        }
        "doctor" => {
            if args.len() <= 2 {
                error_then_help("You must supply the rustdesk-server address");
            }
            doctor(args[2].as_str());
        }
        _ => print_help(),
    }
}
