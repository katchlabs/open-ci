locals {
  common_tags = merge(
    {
      "open-ci-managed" = "true"
      "open-ci-role"    = "controller"
    },
    var.tags,
  )

  controller_user_data = <<-EOF
    #cloud-config
    users:
      - default
      - name: open-ci
        system: true
        shell: /usr/sbin/nologin
        home: /var/lib/open-ci
        create_home: false
    runcmd:
      - install -d -m 0750 -o open-ci -g open-ci /etc/open-ci
      - install -d -m 0750 -o open-ci -g open-ci /var/lib/open-ci
      - install -d -m 0750 -o open-ci -g open-ci /run/open-ci
  EOF
}

data "aws_caller_identity" "current" {}

data "aws_region" "current" {}

data "aws_partition" "current" {}

data "aws_ami" "ubuntu" {
  count       = var.ami_id == "" ? 1 : 0
  most_recent = true
  owners      = ["099720109477"]

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-${var.architecture == "arm64" ? "arm64" : "amd64"}-server-*"]
  }

  filter {
    name   = "architecture"
    values = [var.architecture]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }

  filter {
    name   = "root-device-type"
    values = ["ebs"]
  }
}

data "aws_iam_policy_document" "ec2_assume_role" {
  statement {
    sid     = "AllowEc2ControllerAssumeRole"
    effect  = "Allow"
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "controller" {
  name               = "${var.name_prefix}-controller"
  assume_role_policy = data.aws_iam_policy_document.ec2_assume_role.json
  tags               = local.common_tags
}

resource "aws_iam_instance_profile" "controller" {
  name = "${var.name_prefix}-controller"
  role = aws_iam_role.controller.name
  tags = local.common_tags
}

resource "aws_iam_role_policy_attachment" "ssm_session_manager" {
  count      = var.enable_ssm ? 1 : 0
  role       = aws_iam_role.controller.name
  policy_arn = "arn:${data.aws_partition.current.partition}:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_role_policy_attachment" "external_controller_policy" {
  count      = var.controller_policy_arn != "" ? 1 : 0
  role       = aws_iam_role.controller.name
  policy_arn = var.controller_policy_arn
}

data "aws_iam_policy_document" "runner_lifecycle" {
  count = var.controller_policy_arn == "" ? 1 : 0

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
    resources = ["arn:aws:ec2:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:instance/*"]

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
    resources = ["arn:aws:ec2:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:volume/*"]

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
    resources = [
      "arn:aws:ec2:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:network-interface/*",
      "arn:aws:ec2:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:security-group/*",
      "arn:aws:ec2:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:subnet/*",
      "arn:aws:ec2:${data.aws_region.current.name}::image/*",
      "arn:aws:ec2:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:image/*",
    ]
  }

  statement {
    sid     = "TagRunnersAtLaunchOnly"
    effect  = "Allow"
    actions = ["ec2:CreateTags"]
    resources = [
      "arn:aws:ec2:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:instance/*",
      "arn:aws:ec2:${data.aws_region.current.name}:${data.aws_caller_identity.current.account_id}:volume/*",
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

resource "aws_iam_role_policy" "runner_lifecycle" {
  count  = var.controller_policy_arn == "" ? 1 : 0
  name   = "${var.name_prefix}-controller-ec2-lifecycle"
  role   = aws_iam_role.controller.id
  policy = data.aws_iam_policy_document.runner_lifecycle[0].json
}

data "aws_iam_policy_document" "cross_account_assume_role" {
  count = length(var.assume_role_arns) > 0 ? 1 : 0

  statement {
    sid       = "AssumeCrossAccountRunnerRoles"
    effect    = "Allow"
    actions   = ["sts:AssumeRole"]
    resources = var.assume_role_arns
  }
}

resource "aws_iam_role_policy" "cross_account_assume_role" {
  count  = length(var.assume_role_arns) > 0 ? 1 : 0
  name   = "${var.name_prefix}-controller-sts-assume-role"
  role   = aws_iam_role.controller.id
  policy = data.aws_iam_policy_document.cross_account_assume_role[0].json
}

data "aws_iam_policy_document" "backup_bucket" {
  count = var.backup_bucket_arn != "" ? 1 : 0

  statement {
    sid       = "ListBackupBucket"
    effect    = "Allow"
    actions   = ["s3:ListBucket"]
    resources = [var.backup_bucket_arn]
  }

  statement {
    sid       = "ReadWriteBackupObjects"
    effect    = "Allow"
    actions   = ["s3:GetObject", "s3:PutObject"]
    resources = ["${var.backup_bucket_arn}/*"]
  }
}

resource "aws_iam_role_policy" "backup_bucket" {
  count  = var.backup_bucket_arn != "" ? 1 : 0
  name   = "${var.name_prefix}-controller-s3-backups"
  role   = aws_iam_role.controller.id
  policy = data.aws_iam_policy_document.backup_bucket[0].json
}

resource "aws_security_group" "controller" {
  name        = "${var.name_prefix}-controller"
  description = "Allow HTTPS (443) and HTTP (80 for ACME) to Caddy; keep port 8080 private on loopback."
  vpc_id      = var.vpc_id

  ingress {
    description = "HTTPS ingress for GitHub webhooks, runner bootstrap, and operator API"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = var.ingress_cidrs
  }

  ingress {
    description = "HTTP ingress for ACME HTTP-01 TLS challenge and HTTPS redirect"
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = var.ingress_cidrs
  }

  egress {
    description      = "Outbound HTTPS/DNS for AWS APIs, GitHub APIs, and package updates"
    from_port        = 0
    to_port          = 0
    protocol         = "-1"
    cidr_blocks      = ["0.0.0.0/0"]
    ipv6_cidr_blocks = ["::/0"]
  }

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-controller-sg"
  })
}

resource "aws_instance" "controller" {
  ami                         = var.ami_id != "" ? var.ami_id : data.aws_ami.ubuntu[0].id
  instance_type               = var.instance_type
  subnet_id                   = var.subnet_id
  vpc_security_group_ids      = [aws_security_group.controller.id]
  iam_instance_profile        = aws_iam_instance_profile.controller.name
  associate_public_ip_address = true
  user_data                   = local.controller_user_data
  user_data_replace_on_change = false

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_type           = "gp3"
    volume_size           = var.root_volume_size_gb
    encrypted             = true
    delete_on_termination = true
  }

  tags = merge(local.common_tags, {
    Name           = "${var.name_prefix}-controller"
    open-ci-domain = var.domain
  })

  volume_tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-controller-root"
  })
}

resource "aws_eip" "controller" {
  domain   = "vpc"
  instance = aws_instance.controller.id

  tags = merge(local.common_tags, {
    Name = "${var.name_prefix}-controller-eip"
  })
}
