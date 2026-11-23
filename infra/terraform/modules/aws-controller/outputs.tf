output "controller_instance_id" {
  description = "EC2 instance ID of the open-ci controller host."
  value       = aws_instance.controller.id
}

output "controller_ip" {
  description = "Static Elastic IPv4 address for DNS A record pointing to open-ci."
  value       = aws_eip.controller.public_ip
}

output "controller_iam_role_arn" {
  description = "IAM role ARN attached to the open-ci controller instance profile."
  value       = aws_iam_role.controller.arn
}

output "controller_iam_role_name" {
  description = "IAM role name attached to the open-ci controller instance profile."
  value       = aws_iam_role.controller.name
}

output "controller_instance_profile_name" {
  description = "EC2 instance profile name attached to the open-ci controller."
  value       = aws_iam_instance_profile.controller.name
}

output "controller_security_group_id" {
  description = "Security group ID attached to the open-ci controller."
  value       = aws_security_group.controller.id
}

output "public_url" {
  description = "Base HTTPS URL for the open-ci control plane."
  value       = "https://${var.domain}"
}

output "ssm_session_command" {
  description = "Command that opens an operator shell on the controller through SSM Session Manager."
  value       = var.enable_ssm ? "aws ssm start-session --region ${data.aws_region.current.name} --target ${aws_instance.controller.id}" : ""
}
