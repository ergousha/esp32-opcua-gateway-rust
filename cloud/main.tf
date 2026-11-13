# The gateway's own cloud resources, outside the platform's state (plan §7).
#
# One rule, off by default: OPC UA telemetry batches, which the gateway
# publishes on dt/<thing>/opcua while its `platform` shadow has
# desired.telemetry.enabled, to the log group gateway-hil reads. Every
# message the rule takes costs a rule, an action and Logs ingestion, so it
# runs only for a debugging session or a HIL run:
#
#   terraform apply -var debug_telemetry_logs=true   # then without, after
#
# The platform's kill switch disables it with every other gateway_* rule
# when the guard trips; a guard reset enables it again only if it was on.

# Created by the platform before products had a cloud/ root; gateway-hil
# reads it by this name (gateway-hil/src/cloud.rs). The platform state let go
# of it with a `removed` block; this adopts it.
import {
  to = aws_cloudwatch_log_group.opcua_telemetry
  id = "/esp32-ztp/opcua-telemetry"
}

resource "aws_cloudwatch_log_group" "opcua_telemetry" {
  # Not /gateway/*: gateway-hil and the HIL runbooks know this name.
  # tflint-ignore: opa_deny_names
  name              = "/esp32-ztp/opcua-telemetry"
  retention_in_days = 3
}

data "aws_iam_policy_document" "rule_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["iot.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "opcua_rule" {
  name               = "gateway-opcua-telemetry-rule"
  assume_role_policy = data.aws_iam_policy_document.rule_assume.json
}

resource "aws_iam_role_policy" "opcua_rule" {
  name = "gateway-opcua-telemetry-rule"
  role = aws_iam_role.opcua_rule.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["logs:CreateLogStream", "logs:PutLogEvents"]
      Resource = "${aws_cloudwatch_log_group.opcua_telemetry.arn}:*"
    }]
  })
}

# A batch is {t, v, d} (gateway-core batcher::Batch); gateway-hil reads v and
# d. The thing name and arrival time are added for reading the logs.
resource "aws_iot_topic_rule" "opcua_telemetry_to_logs" {
  name        = "gateway_opcua_telemetry_to_logs"
  description = "OPC UA telemetry batches -> CloudWatch Logs, for debugging and gateway-hil. Off unless debug_telemetry_logs."
  enabled     = var.debug_telemetry_logs
  sql         = "SELECT t, v, d, topic(2) AS thing, timestamp() AS ts FROM 'dt/+/opcua'"
  sql_version = "2016-03-23"
  tags        = { killable = "true" }

  cloudwatch_logs {
    log_group_name = aws_cloudwatch_log_group.opcua_telemetry.name
    role_arn       = aws_iam_role.opcua_rule.arn
  }
}
