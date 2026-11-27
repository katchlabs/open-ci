data "aws_caller_identity" "current" {}

locals {
  common_tags = merge(
    {
      "open-ci-managed" = "true"
      "open-ci-role"    = "byoc"
    },
    var.tags,
  )
  subnets_by_az = var.create_vpc ? {
    for idx, az in var.availability_zones : az => cidrsubnet(var.vpc_cidr, 4, idx)
  } : {}
  vpc_id     = var.create_vpc ? aws_vpc.runners[0].id : var.existing_vpc_id
  subnet_ids = var.create_vpc ? [for s in values(aws_subnet.runners) : s.id] : var.existing_subnet_ids
  subnet_arns = var.create_vpc ? [for s in values(aws_subnet.runners) : s.arn] : [
    for id in var.existing_subnet_ids : "arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:subnet/${id}"
  ]
}

resource "aws_vpc" "runners" {
  count                = var.create_vpc ? 1 : 0
  cidr_block           = var.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-byoc-runners-vpc"
  })
}

resource "aws_internet_gateway" "runners" {
  count  = var.create_vpc ? 1 : 0
  vpc_id = aws_vpc.runners[0].id

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-byoc-runners-igw"
  })
}

resource "aws_subnet" "runners" {
  for_each                = local.subnets_by_az
  vpc_id                  = aws_vpc.runners[0].id
  cidr_block              = each.value
  availability_zone       = each.key
  map_public_ip_on_launch = true

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-byoc-runners-${each.key}"
  })
}

resource "aws_route_table" "runners" {
  count  = var.create_vpc ? 1 : 0
  vpc_id = aws_vpc.runners[0].id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.runners[0].id
  }

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-byoc-runners-rt"
  })
}

resource "aws_route_table_association" "runners" {
  for_each       = aws_subnet.runners
  subnet_id      = each.value.id
  route_table_id = aws_route_table.runners[0].id
}

resource "aws_security_group" "runners" {
  name        = "${var.name_prefix}-byoc-runners-egress-only"
  description = "Deny all inbound traffic; allow outbound CI egress only."
  vpc_id      = local.vpc_id

  egress {
    description      = "Outbound HTTPS/HTTP/DNS/SSH for CI jobs"
    from_port        = 0
    to_port          = 0
    protocol         = "-1"
    cidr_blocks      = ["0.0.0.0/0"]
    ipv6_cidr_blocks = ["::/0"]
  }

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-byoc-runners-sg"
  })

  lifecycle {
    precondition {
      condition     = var.create_vpc || (var.existing_vpc_id != "" && length(var.existing_subnet_ids) > 0)
      error_message = "When create_vpc is false, both existing_vpc_id and at least one entry in existing_subnet_ids are required."
    }
  }
}

resource "aws_iam_service_linked_role" "spot" {
  count            = var.create_spot_service_linked_role ? 1 : 0
  aws_service_name = "spot.amazonaws.com"
  description      = "Lets EC2 launch and manage open-ci Spot runners."
  tags             = local.common_tags
}

resource "aws_s3_bucket" "cache" {
  count  = var.create_cache_bucket ? 1 : 0
  bucket = "${var.name_prefix}-cache-${data.aws_caller_identity.current.account_id}-${var.aws_region}"
  tags   = local.common_tags
}

resource "aws_s3_bucket_public_access_block" "cache" {
  count                   = var.create_cache_bucket ? 1 : 0
  bucket                  = aws_s3_bucket.cache[0].id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "cache" {
  count  = var.create_cache_bucket ? 1 : 0
  bucket = aws_s3_bucket.cache[0].id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "cache" {
  count  = var.create_cache_bucket ? 1 : 0
  bucket = aws_s3_bucket.cache[0].id

  rule {
    id     = "expire-old-cache-objects"
    status = "Enabled"

    expiration {
      days = var.cache_retention_days
    }
  }
}

data "aws_iam_policy_document" "byoc_trust" {
  statement {
    sid     = "AllowHostedControlPlaneAssumeRoleWithExternalId"
    effect  = "Allow"
    actions = ["sts:AssumeRole"]

    principals {
      type        = "AWS"
      identifiers = [var.trusted_principal_arn]
    }

    condition {
      test     = "StringEquals"
      variable = "sts:ExternalId"
      values   = [var.external_id]
    }
  }
}

data "aws_iam_policy_document" "byoc_permissions" {
  statement {
    sid    = "DescribeInventoryAndPreflight"
    effect = "Allow"
    actions = [
      "ec2:DescribeAvailabilityZones",
      "ec2:DescribeImages",
      "ec2:DescribeInstances",
      "ec2:DescribeSecurityGroups",
      "ec2:DescribeSpotPriceHistory",
      "ec2:DescribeSubnets",
      "ec2:DescribeVolumes",
      "servicequotas:GetServiceQuota",
    ]
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
      local.subnet_arns,
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

  dynamic "statement" {
    for_each = var.create_cache_bucket ? [1] : []
    content {
      sid    = "ScopedCacheBucketAccess"
      effect = "Allow"
      actions = [
        "s3:AbortMultipartUpload",
        "s3:DeleteObject",
        "s3:GetObject",
        "s3:ListBucket",
        "s3:PutObject",
      ]
      resources = [
        aws_s3_bucket.cache[0].arn,
        "${aws_s3_bucket.cache[0].arn}/*",
      ]
    }
  }
}

resource "aws_iam_role" "byoc" {
  name                 = "${var.name_prefix}-byoc-runner-manager"
  description          = "Cross-account BYOC role assumed by open-ci with ExternalId to launch zero-IAM ephemeral Spot runners."
  assume_role_policy   = data.aws_iam_policy_document.byoc_trust.json
  max_session_duration = 3600
  tags                 = local.common_tags
}

resource "aws_iam_policy" "byoc" {
  name        = "${var.name_prefix}-byoc-ec2-controller"
  description = "Least-privilege EC2 runner policy scoped to open-ci-managed=true with IMDSv2 required and zero runner instance profiles."
  policy      = data.aws_iam_policy_document.byoc_permissions.json
  tags        = local.common_tags
}

resource "aws_iam_role_policy_attachment" "byoc" {
  role       = aws_iam_role.byoc.name
  policy_arn = aws_iam_policy.byoc.arn
}
