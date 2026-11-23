locals {
  common_tags = merge(
    {
      "open-ci-managed" = "true"
      "open-ci-role"    = "foundation"
    },
    var.tags,
  )
  subnets_by_az = {
    for idx, az in var.availability_zones : az => cidrsubnet(var.vpc_cidr, 4, idx)
  }
}

resource "aws_vpc" "runners" {
  cidr_block           = var.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-runners-vpc"
  })
}

resource "aws_internet_gateway" "runners" {
  vpc_id = aws_vpc.runners.id

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-runners-igw"
  })
}

resource "aws_subnet" "runners" {
  for_each                = local.subnets_by_az
  vpc_id                  = aws_vpc.runners.id
  cidr_block              = each.value
  availability_zone       = each.key
  map_public_ip_on_launch = true

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-runners-${each.key}"
  })
}

resource "aws_route_table" "runners" {
  vpc_id = aws_vpc.runners.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.runners.id
  }

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-runners-rt"
  })
}

resource "aws_route_table_association" "runners" {
  for_each       = aws_subnet.runners
  subnet_id      = each.value.id
  route_table_id = aws_route_table.runners.id
}

resource "aws_security_group" "runners" {
  name        = "${var.name_prefix}-runners-egress-only"
  description = "Deny all inbound traffic; allow outbound CI egress only."
  vpc_id      = aws_vpc.runners.id

  egress {
    description      = "Outbound HTTPS/HTTP/DNS/SSH for CI jobs"
    from_port        = 0
    to_port          = 0
    protocol         = "-1"
    cidr_blocks      = ["0.0.0.0/0"]
    ipv6_cidr_blocks = ["::/0"]
  }

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-runners-sg"
  })
}

data "aws_caller_identity" "current" {}

# RunInstances only launches Spot through the API once this account-wide role exists.
resource "aws_iam_service_linked_role" "spot" {
  count            = var.create_spot_service_linked_role ? 1 : 0
  aws_service_name = "spot.amazonaws.com"
  description      = "Lets EC2 launch and manage open-ci Spot runners."
  tags             = local.common_tags
}

data "aws_iam_policy_document" "controller" {
  statement {
    sid       = "DescribeInventory"
    effect    = "Allow"
    actions   = ["ec2:DescribeInstances", "ec2:DescribeVolumes"]
    resources = ["*"]
  }

  statement {
    sid       = "LaunchTaggedRunnerInstances"
    effect    = "Allow"
    actions   = ["ec2:RunInstances"]
    resources = ["arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:instance/*"]

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/open-ci-managed"
      values   = ["true"]
    }

    condition {
      test     = "StringEquals"
      variable = "ec2:MetadataHttpTokens"
      values   = ["required"]
    }
  }

  statement {
    sid       = "LaunchTaggedRunnerVolumes"
    effect    = "Allow"
    actions   = ["ec2:RunInstances"]
    resources = ["arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:volume/*"]

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/open-ci-managed"
      values   = ["true"]
    }
  }

  statement {
    sid     = "UseRunnerNetworkAndImage"
    effect  = "Allow"
    actions = ["ec2:RunInstances"]
    resources = concat(
      [
        "arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:network-interface/*",
        aws_security_group.runners.arn,
        "arn:aws:ec2:${var.aws_region}::image/*",
        "arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:image/*",
      ],
      [for s in values(aws_subnet.runners) : s.arn],
    )
  }

  statement {
    sid     = "TagRunnersAtLaunchOnly"
    effect  = "Allow"
    actions = ["ec2:CreateTags"]
    resources = [
      "arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:instance/*",
      "arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:volume/*",
    ]

    condition {
      test     = "StringEquals"
      variable = "ec2:CreateAction"
      values   = ["RunInstances"]
    }

    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/open-ci-managed"
      values   = ["true"]
    }
  }

  statement {
    sid       = "TerminateOnlyOpenCiResources"
    effect    = "Allow"
    actions   = ["ec2:TerminateInstances", "ec2:DeleteVolume"]
    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "aws:ResourceTag/open-ci-managed"
      values   = ["true"]
    }
  }
}

resource "aws_iam_policy" "controller" {
  name        = "${var.name_prefix}-ec2-controller"
  description = "Least-privilege EC2 runner management policy scoped to open-ci-managed=true."
  policy      = data.aws_iam_policy_document.controller.json
  tags        = local.common_tags
}

data "aws_iam_policy_document" "gcp_controller_federation" {
  count = var.federate_gcp_controller ? 1 : 0

  statement {
    sid     = "GcpControllerServiceAccountOnly"
    effect  = "Allow"
    actions = ["sts:AssumeRoleWithWebIdentity"]

    principals {
      type        = "Federated"
      identifiers = ["accounts.google.com"]
    }

    condition {
      test     = "StringEquals"
      variable = "accounts.google.com:aud"
      values   = [var.gcp_controller_service_account_unique_id]
    }

    condition {
      test     = "StringEquals"
      variable = "accounts.google.com:sub"
      values   = [var.gcp_controller_service_account_unique_id]
    }

    condition {
      test     = "StringEquals"
      variable = "accounts.google.com:oaud"
      values   = [var.web_identity_audience]
    }
  }
}

resource "aws_iam_role" "gcp_controller" {
  count                = var.federate_gcp_controller ? 1 : 0
  name                 = "${var.name_prefix}-gcp-controller"
  description          = "Assumed keylessly by the GCP-hosted open-ci controller to manage AWS runners."
  assume_role_policy   = data.aws_iam_policy_document.gcp_controller_federation[0].json
  max_session_duration = 3600
  tags                 = local.common_tags

  lifecycle {
    precondition {
      condition     = can(regex("^[0-9]{10,32}$", var.gcp_controller_service_account_unique_id))
      error_message = "federate_gcp_controller requires gcp_controller_service_account_unique_id (the numeric service account unique ID)."
    }
  }
}

resource "aws_iam_role_policy_attachment" "gcp_controller" {
  count      = var.federate_gcp_controller ? 1 : 0
  role       = aws_iam_role.gcp_controller[0].name
  policy_arn = aws_iam_policy.controller.arn
}

data "aws_iam_policy_document" "janitor_assume" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

data "aws_iam_policy_document" "janitor" {
  statement {
    sid       = "DescribeTaggedInventory"
    effect    = "Allow"
    actions   = ["ec2:DescribeInstances", "ec2:DescribeVolumes"]
    resources = ["*"]
  }

  statement {
    sid       = "CleanupOnlyExpiredOpenCiResources"
    effect    = "Allow"
    actions   = ["ec2:TerminateInstances", "ec2:DeleteVolume"]
    resources = ["*"]

    condition {
      test     = "StringEquals"
      variable = "aws:ResourceTag/open-ci-managed"
      values   = ["true"]
    }
  }

  statement {
    sid    = "CloudWatchLogs"
    effect = "Allow"
    actions = [
      "logs:CreateLogGroup",
      "logs:CreateLogStream",
      "logs:PutLogEvents",
    ]
    resources = ["arn:aws:logs:${var.aws_region}:${data.aws_caller_identity.current.account_id}:log-group:/aws/lambda/${var.name_prefix}-aws-janitor:*"]
  }
}

resource "aws_iam_role" "janitor" {
  count              = var.janitor_zip_path != "" ? 1 : 0
  name               = "${var.name_prefix}-aws-janitor"
  assume_role_policy = data.aws_iam_policy_document.janitor_assume.json
  tags               = local.common_tags
}

resource "aws_iam_role_policy" "janitor" {
  count  = var.janitor_zip_path != "" ? 1 : 0
  name   = "${var.name_prefix}-aws-janitor"
  role   = aws_iam_role.janitor[0].id
  policy = data.aws_iam_policy_document.janitor.json
}

resource "aws_lambda_function" "janitor" {
  count            = var.janitor_zip_path != "" ? 1 : 0
  function_name    = "${var.name_prefix}-aws-janitor"
  role             = aws_iam_role.janitor[0].arn
  runtime          = "provided.al2023"
  architectures    = ["arm64"]
  handler          = "bootstrap"
  filename         = var.janitor_zip_path
  source_code_hash = filebase64sha256(var.janitor_zip_path)
  timeout          = 30
  memory_size      = 128

  environment {
    variables = {
      OPEN_CI_AWS_ACCOUNT_ID      = data.aws_caller_identity.current.account_id
      JANITOR_DRY_RUN             = tostring(var.janitor_dry_run)
      JANITOR_MAX_CEILING_SECONDS = tostring(var.janitor_max_lifetime_seconds)
    }
  }

  tags = local.common_tags
}

resource "aws_cloudwatch_event_rule" "janitor_schedule" {
  count               = var.janitor_zip_path != "" ? 1 : 0
  name                = "${var.name_prefix}-aws-janitor-1m"
  description         = "Invoke the Rust open-ci AWS expiry janitor every minute."
  schedule_expression = "rate(1 minute)"
  tags                = local.common_tags
}

resource "aws_cloudwatch_event_target" "janitor" {
  count = var.janitor_zip_path != "" ? 1 : 0
  rule  = aws_cloudwatch_event_rule.janitor_schedule[0].name
  arn   = aws_lambda_function.janitor[0].arn
}

resource "aws_lambda_permission" "janitor_events" {
  count         = var.janitor_zip_path != "" ? 1 : 0
  statement_id  = "AllowEventBridgeInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.janitor[0].function_name
  principal     = "events.amazonaws.com"
  source_arn    = aws_cloudwatch_event_rule.janitor_schedule[0].arn
}

resource "aws_cloudwatch_metric_alarm" "janitor_errors" {
  count               = var.janitor_zip_path != "" ? 1 : 0
  alarm_name          = "${var.name_prefix}-aws-janitor-errors"
  alarm_description   = "Alert when the open-ci AWS expiry janitor fails to execute cleanly."
  namespace           = "AWS/Lambda"
  metric_name         = "Errors"
  statistic           = "Sum"
  period              = 300
  evaluation_periods  = 1
  threshold           = 1
  comparison_operator = "GreaterThanOrEqualToThreshold"
  treat_missing_data  = "breaching"
  alarm_actions       = var.alarm_sns_topic_arn != "" ? [var.alarm_sns_topic_arn] : []

  dimensions = {
    FunctionName = aws_lambda_function.janitor[0].function_name
  }

  tags = local.common_tags
}
