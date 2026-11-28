packer {
  required_version = ">= 1.10.0"

  required_plugins {
    googlecompute = {
      source  = "github.com/hashicorp/googlecompute"
      version = "~> 1.1"
    }
    amazon = {
      source  = "github.com/hashicorp/amazon"
      version = "~> 1.3"
    }
  }
}

variable "arch" {
  type        = string
  default     = "x64"
  description = "Target CPU architecture: x64 or arm64."
  validation {
    condition     = contains(["x64", "arm64"], var.arch)
    error_message = "The arch variable must be x64 or arm64."
  }
}

variable "github_runner_version" {
  type        = string
  default     = "2.338.2"
  description = "Pinned actions/runner release version."
}

variable "open_ci_version" {
  type        = string
  default     = "0.2.2"
  description = "open-ci release baked into the image; pins the GCP image name referenced by `open-ci init`."
}

variable "github_runner_sha256" {
  type        = string
  description = "Pinned SHA-256 checksum for the actions/runner archive."
}

variable "open_ci_binary_path" {
  type        = string
  description = "Path to the cross-compiled open-ci binary for the target architecture."
}

variable "gcp_project_id" {
  type        = string
  default     = ""
  description = "GCP project ID when building the googlecompute target."
}

variable "gcp_zone" {
  type        = string
  default     = "us-central1-a"
  description = "GCP zone used for the temporary Packer builder VM."
}

variable "gcp_machine_type" {
  type        = string
  default     = ""
  description = "Optional override for the temporary GCP builder machine type (defaults by arch)."
}

variable "gcp_disk_type" {
  type        = string
  default     = ""
  description = "Optional GCP boot disk type override (auto-selects hyperdisk-balanced for c4*/n4* machines, pd-ssd otherwise)."
}

variable "aws_region" {
  type        = string
  default     = "us-east-1"
  description = "AWS region used for the temporary Packer builder VM."
}

variable "aws_instance_type" {
  type        = string
  default     = ""
  description = "Optional override for the temporary AWS builder instance type (defaults by arch)."
}

variable "aws_volume_type" {
  type        = string
  default     = "gp3"
  description = "AWS EBS volume type for the built AMI."
}

variable "aws_vpc_id" {
  type        = string
  default     = ""
  description = "Optional AWS VPC ID for the temporary Packer builder (leave empty for default VPC)."
}

variable "aws_subnet_id" {
  type        = string
  default     = ""
  description = "Optional AWS subnet ID for the temporary Packer builder."
}

variable "disk_size_gb" {
  type        = number
  default     = 30
  description = "Base image root volume size in GiB."
}

variable "install_docker" {
  type        = bool
  default     = true
  description = "Whether to pre-install Docker Engine and compose on the runner image."
}

locals {
  image_timestamp = formatdate("YYYYMMDD-hhmmss", timestamp())
  image_family    = "open-ci-ubuntu-2404-${var.arch}"
  image_name      = "${local.image_family}-v${replace(var.open_ci_version, ".", "-")}-r${replace(var.github_runner_version, ".", "-")}"
  gcp_machine     = var.gcp_machine_type != "" ? var.gcp_machine_type : (var.arch == "arm64" ? "t2a-standard-4" : "n2d-standard-4")
  gcp_disk        = var.gcp_disk_type != "" ? var.gcp_disk_type : (startswith(local.gcp_machine, "c4") || startswith(local.gcp_machine, "n4") ? "hyperdisk-balanced" : "pd-ssd")
  gcp_source      = var.arch == "arm64" ? "ubuntu-2404-lts-arm64" : "ubuntu-2404-lts-amd64"
  aws_instance    = var.aws_instance_type != "" ? var.aws_instance_type : (var.arch == "arm64" ? "c7g.xlarge" : "c7a.xlarge")
  aws_arch        = var.arch == "arm64" ? "arm64" : "x86_64"
}

source "googlecompute" "ubuntu_runner" {
  project_id          = var.gcp_project_id
  zone                = var.gcp_zone
  source_image_family = local.gcp_source
  image_name          = local.image_name
  image_family        = local.image_family
  machine_type        = local.gcp_machine
  disk_size           = var.disk_size_gb
  disk_type           = local.gcp_disk
  ssh_username        = "packer"
  image_labels = {
    open-ci-managed = "true"
    open-ci-arch    = var.arch
    open-ci-built   = lower(local.image_timestamp)
  }
}

source "amazon-ebs" "ubuntu_runner" {
  region        = var.aws_region
  instance_type = local.aws_instance
  vpc_id        = var.aws_vpc_id != "" ? var.aws_vpc_id : null
  subnet_id     = var.aws_subnet_id != "" ? var.aws_subnet_id : null
  ssh_username  = "ubuntu"
  ami_name      = "${local.image_name}-${local.image_timestamp}"

  source_ami_filter {
    filters = {
      name                = "ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-${var.arch == "arm64" ? "arm64" : "amd64"}-server-*"
      root-device-type    = "ebs"
      virtualization-type = "hvm"
      architecture        = local.aws_arch
    }
    most_recent = true
    owners      = ["099720109477"] # Canonical
  }

  launch_block_device_mappings {
    device_name           = "/dev/sda1"
    volume_size           = var.disk_size_gb
    volume_type           = var.aws_volume_type
    delete_on_termination = true
  }

  tags = {
    "open-ci-managed" = "true"
    "open-ci-arch"    = var.arch
    "open-ci-family"  = local.image_family
  }
}

build {
  name = "open-ci-runner"

  sources = [
    "source.googlecompute.ubuntu_runner",
    "source.amazon-ebs.ubuntu_runner",
  ]

  provisioner "file" {
    source      = var.open_ci_binary_path
    destination = "/tmp/open-ci"
  }

  provisioner "file" {
    source      = "${path.root}/scripts/setup-nvme.sh"
    destination = "/tmp/setup-nvme.sh"
  }

  provisioner "file" {
    source      = "${path.root}/scripts/tune-host.sh"
    destination = "/tmp/tune-host.sh"
  }

  provisioner "shell" {
    execute_command = "sudo -E bash '{{ .Path }}'"
    environment_vars = [
      "RUNNER_ARCH=${var.arch}",
      "RUNNER_VERSION=${var.github_runner_version}",
      "RUNNER_SHA256=${var.github_runner_sha256}",
      "INSTALL_DOCKER=${var.install_docker}",
    ]
    script = "${path.root}/scripts/install-runner.sh"
  }
}
