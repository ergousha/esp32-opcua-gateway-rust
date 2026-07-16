# ---------------------------------------------------------------------------
# Lambda (pre-provisioning hook) rolu: DynamoDB oku/guncelle + loglar.
# ---------------------------------------------------------------------------
data "aws_iam_policy_document" "lambda_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "hook" {
  name               = "${var.project_name}-hook-role"
  assume_role_policy = data.aws_iam_policy_document.lambda_assume.json
}

data "aws_iam_policy_document" "hook_policy" {
  statement {
    sid       = "DynamoDBAccess"
    actions   = ["dynamodb:GetItem", "dynamodb:UpdateItem"]
    resources = [aws_dynamodb_table.device_registry.arn]
  }

  statement {
    sid = "Logs"
    actions = [
      "logs:CreateLogStream",
      "logs:PutLogEvents",
    ]
    resources = ["${aws_cloudwatch_log_group.hook.arn}:*"]
  }
}

resource "aws_iam_role_policy" "hook" {
  name   = "${var.project_name}-hook-policy"
  role   = aws_iam_role.hook.id
  policy = data.aws_iam_policy_document.hook_policy.json
}

# ---------------------------------------------------------------------------
# IoT Core'un provisioning sirasinda ustlendigi rol (thing/cert/policy olusturur).
# ---------------------------------------------------------------------------
data "aws_iam_policy_document" "iot_provisioning_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["iot.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "provisioning" {
  name               = "${var.project_name}-provisioning-role"
  assume_role_policy = data.aws_iam_policy_document.iot_provisioning_assume.json
}

# AWS yonetilen politika: fleet provisioning registration icin gereken izinler.
resource "aws_iam_role_policy_attachment" "provisioning" {
  role       = aws_iam_role.provisioning.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSIoTThingsRegistration"
}
