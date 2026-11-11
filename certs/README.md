# Certificates

Only `AmazonRootCA1.pem` is used: the public Amazon root CA that AWS IoT's
server certificate chains to. The device gets its copy from the platform SDK
([`iot-device-sdk`](https://github.com/ergousha/iot-device-sdk), compiled into
`device-sdk`); this one is the PEM that `scripts/scan-image.py` allows in a
release image, which must hold no other certificate and no private key.

```sh
curl -s https://www.amazontrust.com/repository/AmazonRootCA1.pem > certs/AmazonRootCA1.pem
```

Images carry no claim identity. The claim certificate and key reach a gateway
only at the bench, over its serial console (`ergo commission`, see the
[README](../README.md#commissioning-a-gateway)), and are erased from it once it
has enrolled. `certs/claim.crt.pem` and `certs/claim.private.key` from older
checkouts are no longer read by anything and can be deleted.
