//! The git remotes' test double (M13): a bare repository served by git's own server on loopback
//! HTTP and through an ssh stand-in, and the merge methods of a hosting service done with git's
//! plumbing, so that the remote tests never need the network. Written by the M13 plan's task C2.
