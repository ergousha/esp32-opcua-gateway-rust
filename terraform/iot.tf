# ===========================================================================
# AWS IoT Core - Fleet Provisioning by Claim
# ===========================================================================

# ---------------------------------------------------------------------------
# 1) CLAIM (bootstrap) certificate.
#    COMMON certificate carried by all devices leaving the factory.
#    Can only access provisioning MQTT topics; cannot send telemetry.
#    active=true + no csr => AWS generates a keypair, private_key is returned in outputs.
# ---------------------------------------------------------------------------
resource "aws_iot_certificate" "claim" {
  active = true
}

resource "aws_iot_policy" "claim" {
  name = "${var.project_name}-claim-policy"

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid      = "Connect"
        Effect   = "Allow"
        Action   = "iot:Connect"
        Resource = "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:client/*"
      },
      {
        Sid    = "CreateKeysAndCertificate"
        Effect = "Allow"
        Action = ["iot:Publish", "iot:Receive"]
        Resource = [
          "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:topic/$aws/certificates/create/*",
          "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:topic/$aws/provisioning-templates/${var.provisioning_template_name}/provision/*",
        ]
      },
      {
        Sid    = "SubscribeProvisioningResponses"
        Effect = "Allow"
        Action = "iot:Subscribe"
        Resource = [
          "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:topicfilter/$aws/certificates/create/*",
          "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:topicfilter/$aws/provisioning-templates/${var.provisioning_template_name}/provision/*",
        ]
      },
    ]
  })
}

resource "aws_iot_policy_attachment" "claim" {
  policy = aws_iot_policy.claim.name
  target = aws_iot_certificate.claim.arn
}

# ---------------------------------------------------------------------------
# 2) DEVICE policy.
#    Provisioning template binds this to each device's unique certificate.
#    Using policy variables, each device can only access its OWN topics.
# ---------------------------------------------------------------------------
resource "aws_iot_policy" "device" {
  name = "${var.project_name}-device-policy"

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid    = "Connect"
        Effect = "Allow"
        Action = "iot:Connect"
        # ClientId must be the same as thing name (this is how firmware connects).
        Resource = "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:client/$${iot:Connection.Thing.ThingName}"
      },
      {
        Sid    = "PublishTelemetry"
        Effect = "Allow"
        Action = "iot:Publish"
        Resource = [
          "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:topic/${var.telemetry_topic_prefix}/$${iot:Connection.Thing.ThingName}/*",
        ]
      },
      {
        Sid    = "SubscribeCommands"
        Effect = "Allow"
        Action = ["iot:Subscribe"]
        Resource = [
          "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:topicfilter/cmd/$${iot:Connection.Thing.ThingName}/*",
        ]
      },
      {
        Sid    = "ReceiveCommands"
        Effect = "Allow"
        Action = ["iot:Receive"]
        Resource = [
          "arn:aws:iot:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:topic/cmd/$${iot:Connection.Thing.ThingName}/*",
        ]
      },
    ]
  })
}

# ---------------------------------------------------------------------------
# 3) Fleet Provisioning Template.
#    Defines which thing/cert/policy will be created in the RegisterThing call.
#    pre_provisioning_hook: Lambda validation is required on each request.
# ---------------------------------------------------------------------------
resource "aws_iot_provisioning_template" "fleet" {
  name                  = var.provisioning_template_name
  description           = "ESP32-S3 zero-touch fleet provisioning (by claim)"
  provisioning_role_arn = aws_iam_role.provisioning.arn
  enabled               = true

  pre_provisioning_hook {
    target_arn      = aws_lambda_function.pre_provisioning_hook.arn
    payload_version = "2020-04-01"
  }

  template_body = jsonencode({
    Parameters = {
      SerialNumber = { Type = "String" }
      MacAddress   = { Type = "String" }
      # Secret goes to the hook but is not used in the thing resource.
      Secret                      = { Type = "String" }
      "AWS::IoT::Certificate::Id" = { Type = "String" }
    }

    Resources = {
      certificate = {
        Type = "AWS::IoT::Certificate"
        Properties = {
          CertificateId = { Ref = "AWS::IoT::Certificate::Id" }
          Status        = "ACTIVE"
        }
      }

      policy = {
        Type = "AWS::IoT::Policy"
        Properties = {
          PolicyName = aws_iot_policy.device.name
        }
      }

      thing = {
        Type = "AWS::IoT::Thing"
        Properties = {
          ThingName = { Ref = "SerialNumber" }
          AttributePayload = {
            mac = { Ref = "MacAddress" }
          }
        }
        OverrideSettings = {
          AttributePayload = "MERGE"
          ThingTypeName    = "REPLACE"
        }
      }
    }
  })

  # Hook resource-policy must be ready before the template.
  depends_on = [
    aws_lambda_permission.allow_iot,
    aws_iam_role_policy_attachment.provisioning,
  ]
}

# ---------------------------------------------------------------------------
# 4) (Optional) Simple IoT Rule to route telemetry to CloudWatch Logs.
#    Useful to see that the device actually publishes in PoC.
# ---------------------------------------------------------------------------
resource "aws_cloudwatch_log_group" "telemetry" {
  name              = "/${var.project_name}/telemetry"
  retention_in_days = var.log_retention_days
}

data "aws_iam_policy_document" "iot_rule_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["iot.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "iot_rule" {
  name               = "${var.project_name}-telemetry-rule-role"
  assume_role_policy = data.aws_iam_policy_document.iot_rule_assume.json
}

resource "aws_iam_role_policy" "iot_rule" {
  name = "${var.project_name}-telemetry-rule-policy"
  role = aws_iam_role.iot_rule.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["logs:CreateLogStream", "logs:PutLogEvents", "logs:DescribeLogStreams"]
      Resource = "${aws_cloudwatch_log_group.telemetry.arn}:*"
    }]
  })
}

resource "aws_iot_topic_rule" "telemetry_to_logs" {
  name        = replace("${var.project_name}_telemetry_to_logs", "-", "_")
  enabled     = true
  sql         = "SELECT *, topic() AS topic, timestamp() AS ts FROM '${var.telemetry_topic_prefix}/+/data'"
  sql_version = "2016-03-23"

  cloudwatch_logs {
    log_group_name = aws_cloudwatch_log_group.telemetry.name
    role_arn       = aws_iam_role.iot_rule.arn
  }
}
