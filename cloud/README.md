# cloud/

The gateway's own AWS resources, outside the platform's state (platform plan
§7, iot-device-sdk `PLATFORM.md`). State: `products/gateway/cloud.tfstate` in
the platform's state bucket. Applied by the owner, locally; CI only checks
it (`terraform validate`, tflint with the platform's policies).

| Resource | What |
|---|---|
| `gateway_opcua_telemetry_to_logs` | IoT rule: OPC UA telemetry batches (`dt/+/opcua`) to the log group. **Off** unless `debug_telemetry_logs` |
| `/esp32-ztp/opcua-telemetry` | Log group, 3 days. `gateway-hil` reads it. Created by the platform before products had a `cloud/`; adopted here with an `import` block |
| `gateway-opcua-telemetry-rule` | The rule's role: write to that log group only |

```sh
cd cloud
aws-vault exec <owner profile> -- terraform init
aws-vault exec <owner profile> -- terraform apply                                 # rule off
aws-vault exec <owner profile> -- terraform apply -var debug_telemetry_logs=true  # for a HIL run
```

Every message the rule takes costs a rule, an action and Logs ingestion, so
switch it off again after the run. The platform's kill switch disables every
`gateway_*` rule when its guard trips, and a guard reset enables it again only
if it was on.

First apply: after the platform's apply that let go of the log group
(iot-platform, Phase 5). Until then the old rule `esp32_ztp_opcua_telemetry_to_logs`
still feeds the same log group.
