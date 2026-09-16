# Distributed workbench

A fabric connects task origin nodes to the machines that perform their work.

## Language

**Desktop**: One interactive OS session whose foreground window, pointer and keyboard are shared. It is the unit of exclusive GUI acceptance.
_Avoid_: App lock, global GUI lock

**Desktop session**: One bounded acceptance attempt covering activation, observation, interaction, evidence and cleanup while owning a desktop.
_Avoid_: Click lease, workspace driver

**Desktop queue**: The ordered set of sessions waiting for or holding one desktop. All task origins share this queue.
_Avoid_: Per-controller desktop lease

**Quarantined desktop**: A desktop whose previous operation or cleanup outcome is uncertain, so new ownership is withheld pending explicit recovery.
_Avoid_: Idle desktop
