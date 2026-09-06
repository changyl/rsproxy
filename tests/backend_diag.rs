// 后端连接诊断测试
#[cfg(test)]
mod backend_test {
    use bytes::BytesMut;
    use tokio::net::TcpStream;
    use newproxy::proto::{auth, codec, handshake};

    #[tokio::test]
    async fn test_backend_connect_and_query() {
        let host = "127.0.0.1";
        let port = 3306;

        // 1. 连接到 MySQL
        let mut stream = TcpStream::connect((host, port)).await.unwrap();
        stream.set_nodelay(true).unwrap();

        let mut buf = BytesMut::with_capacity(4096);

        // 2. 读取 greeting
        let (_, greeting) = codec::read_packet(&mut stream, &mut buf).await.unwrap();
        println!(
            "greeting: {} bytes, first={:02x}",
            greeting.len(),
            greeting[0]
        );

        let gre = auth::parse_backend_greeting(&greeting).unwrap();
        println!("auth_plugin: {}", gre.auth_plugin_name);

        // 3. 构建 auth response
        let auth_resp = if gre.auth_plugin_name == "caching_sha2_password" {
            println!("using caching_sha2_password");
            auth::build_sha256_auth_response(
                "root",
                "test_password",
                &gre.scramble,
                handshake::BACKEND_CAP_FULL,
                45,
            )
        } else {
            println!("using mysql_native_password");
            auth::build_backend_auth_response("root", "test_password", &gre, 45)
        };

        // 4. 发送 auth
        println!("sending auth: {} bytes", auth_resp.len());
        codec::send_packet(&mut stream, 1, &auth_resp)
            .await
            .unwrap();

        // 5. 读取 auth 结果
        let (_, result) = codec::read_packet(&mut stream, &mut buf).await.unwrap();
        let preview: String = result
            .iter()
            .take(10)
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "auth result: {} bytes, first_bytes=[{}]",
            result.len(),
            preview
        );

        if result.is_empty() || result[0] != 0x00 {
            panic!("auth failed: preview=[{}]", preview);
        }
        println!("auth OK");

        // 6. 发送 COM_QUERY
        let query = b"SELECT 1";
        let mut cmd_pkt = vec![0x03u8]; // COM_QUERY
        cmd_pkt.extend_from_slice(query);
        println!("sending COM_QUERY: {} bytes", cmd_pkt.len());
        codec::send_packet(&mut stream, 0, &cmd_pkt).await.unwrap();

        // 7. 读取响应
        println!("reading response...");
        loop {
            match codec::read_packet(&mut stream, &mut buf).await {
                Ok((seq, pkt)) => {
                    let preview: String = pkt
                        .iter()
                        .take(20)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!(
                        "  packet: seq={}, len={}, first=[{}]",
                        seq,
                        pkt.len(),
                        preview
                    );
                    if !pkt.is_empty() && (pkt[0] == 0x00 || pkt[0] == 0xFF || pkt[0] == 0xFE) {
                        println!("  terminal packet, done");
                        break;
                    }
                }
                Err(e) => {
                    panic!("read error: {}", e);
                }
            }
        }
        println!("test passed!");
    }
}
