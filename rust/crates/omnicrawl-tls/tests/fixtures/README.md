测试专用的证书与私钥夹具。

`tests/session_flow.rs` 用它们做**真实签名与真实验签**，覆盖 ECDSA P-256 与 RSA-2048 两条
CertificateVerify 路径。不用打桩是因为：打桩的签名永远"验得过"，而这层要防的恰恰是
"验不过却没拦住"。

这里的一切都只能用于测试：

- 根证书有效期 100 年，没有吊销入口，公钥已公开出现在本仓库里。
- `leaf-*-key.p8` 是未加密的 PKCS#8 私钥，任何拿到本仓库的人都能用它冒充
  `example.com`——只要对方信任 `ca.der`。
- 除本目录的测试之外，不得把 `ca.der` 加进任何受信根集合。

## 重新生成

```bash
openssl req -x509 -new -key ca-key.pem -sha256 -days 36500 -out ca.pem -config openssl-ca.cnf
openssl x509 -in ca.pem -outform DER -out ca.der

openssl ecparam -name prime256v1 -genkey -noout -out leaf-ec-key.pem
openssl req -new -key leaf-ec-key.pem -out leaf-ec.csr -config openssl-leaf.cnf
openssl x509 -req -in leaf-ec.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
  -days 36500 -sha256 -out leaf-ec.pem -extfile openssl-leaf.cnf -extensions v3_leaf
openssl x509 -in leaf-ec.pem -outform DER -out leaf-ec.der
openssl pkcs8 -topk8 -nocrypt -in leaf-ec-key.pem -outform DER -out leaf-ec-key.p8

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out leaf-rsa-key.pem
openssl req -new -key leaf-rsa-key.pem -out leaf-rsa.csr -config openssl-leaf.cnf
openssl x509 -req -in leaf-rsa.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
  -days 36500 -sha256 -out leaf-rsa.pem -extfile openssl-leaf.cnf -extensions v3_leaf
openssl x509 -in leaf-rsa.pem -outform DER -out leaf-rsa.der
openssl pkcs8 -topk8 -nocrypt -in leaf-rsa-key.pem -outform DER -out leaf-rsa-key.p8
```

`openssl-leaf.cnf` 的 `subjectAltName` 必须包含 `example.com`——`verify_server_chain`
在 `PeerPolicy::VerifyChain` 下会单独校验主机名，漏了它测试会以 `WrongHostName` 失败。
