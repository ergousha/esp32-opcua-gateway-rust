# ===========================================================================
# AWS IoT Core - Fleet Provisioning by Claim
# ===========================================================================

# ---------------------------------------------------------------------------
# 1) CLAIM (bootstrap) sertifikasi.
#    Tum cihazlarin fabrikadan cikarken tasidigi ORTAK sertifika.
#    Sadece provisioning MQTT topic'lerine erisebilir; telemetri gonderemez.
#    active=true + csr yok => AWS keypair uretir, private_key output'ta doner.
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
# 2) CIHAZ (device) politikasi.
#    Provisioning template bunu her cihazin benzersiz sertifikasina baglar.
#    Politika degiskenleriyle her cihaz yalnizca KENDI topic'lerine erisir.
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
        # ClientId, thing adiyla ayni olmali (firmware boyle baglanir).
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
#    RegisterThing cagrisinda hangi thing/cert/policy'nin olusacagini tanimlar.
#    pre_provisioning_hook: her istekte Lambda dogrulamasi zorunlu kilinir.
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
      # Secret hook'a gider ama thing kaynaginda kullanilmaz.
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

  # Hook resource-policy'si template'ten once hazir olmali.
  depends_on = [
    aws_lambda_permission.allow_iot,
    aws_iam_role_policy_attachment.provisioning,
  ]
}

# ---------------------------------------------------------------------------
# 4) (Opsiyonel) Telemetriyi CloudWatch Logs'a dusuren basit bir IoT Rule.
#    PoC'de cihazin gercekten publish ettigini gormek icin faydali.
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
