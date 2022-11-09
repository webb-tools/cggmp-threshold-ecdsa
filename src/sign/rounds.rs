use std::{collections::HashMap, io::Error, marker::PhantomData};

use curv::{
	arithmetic::{traits::*, Modulo, Samplable},
	cryptographic_primitives::hashing::{Digest, DigestExt},
	elliptic::curves::{Curve, Point, Scalar, Secp256k1},
	BigInt,
};
use round_based::{
	containers::{push::Push, BroadcastMsgs, BroadcastMsgsStore},
	Msg,
};
use sha2::Sha256;

use paillier::*;

use crate::{
	presign::{PresigningOutput, PresigningTranscript, DEFAULT_ENCRYPTION_KEY},
	utilities::{
		aff_g::{
			PaillierAffineOpWithGroupComInRangeProof, PaillierAffineOpWithGroupComInRangeStatement,
		},
		dec_q::{
			PaillierDecryptionModQProof, PaillierDecryptionModQStatement,
			PaillierDecryptionModQWitness,
		},
		mul_star::{
			PaillierMultiplicationVersusGroupProof, PaillierMultiplicationVersusGroupStatement,
			PaillierMultiplicationVersusGroupWitness,
		},
	}, ErrorType,
};
use thiserror::Error;

use zeroize::Zeroize;

use super::{SigningBroadcastMessage1, SigningIdentifiableAbortMessage, SigningOutput, SSID};

use super::state_machine::{Round0Messages, Round1Messages};

use rayon::prelude::*;

pub struct Round0 {
	pub ssid: SSID<Secp256k1>,
	pub l: usize, // This is the number of presignings to run in parallel
	pub m: BigInt,
	pub presigning_data:
		HashMap<u16, (PresigningOutput<Secp256k1>, PresigningTranscript<Secp256k1>)>,
}

impl Round0 {
	pub fn proceed<O>(self, mut output: O) -> Result<Round1>
	where
		O: Push<Msg<SigningBroadcastMessage1<Secp256k1>>>,
	{
		// If there is a record for (l,...)
		if let Some((presigning_output, presigning_transcript)) =
			self.presigning_data.get(&(self.l as u16))
		{
			// r = R projected onto x axis
			let r = presigning_output.R.x_coord().unwrap_or(BigInt::zero());
			// sigma_i = k*m + r*chi
			let sigma_i = presigning_output.k_i.mul(&self.m).add(&r.mul(&presigning_output.chi_i));
			let body = SigningBroadcastMessage1 { ssid: self.ssid, i: self.ssid.X.i, sigma_i };
			output.push(Msg { sender: self.ssid.X.i, receiver: None, body });
			// Erase output from memory
			(*presigning_output).zeroize();
			Ok(Round1 {
				ssid: self.ssid,
				i: self.ssid.X.i,
				presigning_transcript: *presigning_transcript,
				m: self.m,
				r,
				sigma_i,
			})
		} else {
			Err(SignError::NoOfflineStageError { l: self.l })
		}
	}
	pub fn is_expensive(&self) -> bool {
		false
	}
}

pub struct Round1 {
	pub ssid: SSID<Secp256k1>,
	pub i: u16,
	pub m: BigInt,
	pub r: BigInt,
	pub sigma_i: BigInt,
	pub presigning_transcript: PresigningTranscript<Secp256k1>,
}

impl Round1 {
	pub fn proceed<O>(
		self,
		input: BroadcastMsgs<SigningBroadcastMessage1<Secp256k1>>,
		mut output: O,
	) -> Result<Round2>
	where
		O: Push<Msg<Option<SigningIdentifiableAbortMessage<Secp256k1>>>>,
	{
		// Mapping from j to sigma_j
		let sigmas: HashMap<u16, BigInt> = HashMap::new();
		for msg in input.into_vec() {
			sigmas.insert(msg.i, msg.sigma_i);
		}
		let sigma: BigInt = sigmas.values().into_iter().fold(self.sigma_i, |acc, x| acc.add(x));

		// Verify (r, sigma) is a valid signature
		// sigma^{-1}
		let sigma_inv = BigInt::mod_inv(&sigma, &self.ssid.q).unwrap();
		// m*sigma^{-1}
		let m_sigma_inv = self.m.mul(&sigma_inv);
		// r*sigma^{-1}
		let r_sigma_inv = self.r.mul(&sigma_inv);
		let g = Point::<Secp256k1>::generator();
		let X = self.ssid.X.public_key();
		let x_projection = ((g * Scalar::from_bigint(&m_sigma_inv)) +
			(X * Scalar::from_bigint(&r_sigma_inv)))
		.x_coord()
		.unwrap_or(BigInt::zero());

		if self.r == x_projection {
			let signing_output = SigningOutput { ssid: self.ssid, m: self.m, r: self.r, sigma };
			output.push(Msg { sender: self.ssid.X.i, receiver: None, body: None });
			Ok(Round2 { ssid: self.ssid, output: Some(signing_output) })
		} else {
			// (l,j) to proof for D_j_i
			let proofs_D_hat_j_i: HashMap<
				(u16, u16),
				PaillierAffineOpWithGroupComInRangeProof<Secp256k1, Sha256>,
			> = HashMap::new();

			// (l,j) to statement for D_j_i
			let statements_D_hat_j_i: HashMap<
				(u16, u16),
				PaillierAffineOpWithGroupComInRangeStatement<Secp256k1, Sha256>,
			> = HashMap::new();

			let D_hat_j_i_map: HashMap<u16, BigInt> = HashMap::new();

			(self.ssid.P, self.ssid.P).par_iter().map(|(j, l)| {
				if *j != self.ssid.X.i && j != l {
					// Compute D_hat_j_i
					let encrypt_minus_beta_hat_i_j = Paillier::encrypt_with_chosen_randomness(
						self.presigning_transcript.eks.get(j).unwrap_or(&DEFAULT_ENCRYPTION_KEY),
						RawPlaintext::from(BigInt::from(-1).mul(
							self.presigning_transcript.beta_hat_i.get(j).unwrap_or(&BigInt::zero()),
						)),
						&Randomness::from(
							self.presigning_transcript.s_hat_i.get(j).unwrap_or(&BigInt::zero()),
						),
					);
					// D_hat_j_i =  (x_i [.] K_j ) ⊕ enc_j(-beta_hat_i_j; s_hat_i_j) where [.] is
					// Paillier multiplication
					let D_hat_j_i =
						*self.presigning_transcript.D_hat_j.get(&self.ssid.X.i).unwrap();

					// F_hat_j_i = enc_i(beta_hat_i_j, r_hat_i_j)
					let F_hat_j_i =
						*self.presigning_transcript.F_hat_j.get(&self.ssid.X.i).unwrap();

					let witness_D_hat_j_i =
						crate::utilities::aff_g::PaillierAffineOpWithGroupComInRangeWitness {
							x: self.presigning_transcript.secrets.x_i,
							y: *self
								.presigning_transcript
								.beta_hat_i
								.get(j)
								.unwrap_or(&BigInt::zero()),
							rho: *self
								.presigning_transcript
								.s_hat_i
								.get(j)
								.unwrap_or(&BigInt::zero()),
							rho_y: *self
								.presigning_transcript
								.r_hat_i
								.get(j)
								.unwrap_or(&BigInt::zero()),
							phantom: PhantomData,
						};
					let statement_D_hat_j_i =
						crate::utilities::aff_g::PaillierAffineOpWithGroupComInRangeStatement {
							S: *self.presigning_transcript.S.get(l).unwrap_or(&BigInt::zero()),
							T: *self.presigning_transcript.T.get(l).unwrap_or(&BigInt::zero()),
							N_hat: *self
								.presigning_transcript
								.N_hats
								.get(l)
								.unwrap_or(&BigInt::zero()),
							N0: self.presigning_transcript.secrets.ek.n,
							N1: self
								.presigning_transcript
								.eks
								.get(j)
								.unwrap_or(&DEFAULT_ENCRYPTION_KEY)
								.n,
							NN0: self.presigning_transcript.secrets.ek.nn,
							NN1: self
								.presigning_transcript
								.eks
								.get(j)
								.unwrap_or(&DEFAULT_ENCRYPTION_KEY)
								.nn,
							C: D_hat_j_i,
							D: *self.presigning_transcript.K.get(j).unwrap_or(&BigInt::zero()),
							Y: F_hat_j_i,
							X: Point::<Secp256k1>::generator().as_point() *
								Scalar::from_bigint(&self.presigning_transcript.secrets.x_i),
							ek_prover: self.presigning_transcript.secrets.ek,
							ek_verifier: *self
								.presigning_transcript
								.eks
								.get(j)
								.unwrap_or(&DEFAULT_ENCRYPTION_KEY),
							phantom: PhantomData,
						};
					let proof_D_hat_j_i =
						crate::utilities::aff_g::PaillierAffineOpWithGroupComInRangeProof::<
							Secp256k1,
							Sha256,
						>::prove(&witness_D_hat_j_i, &statement_D_hat_j_i);
					proofs_D_hat_j_i.insert((*l, *j), proof_D_hat_j_i);
					statements_D_hat_j_i.insert((*l, *j), statement_D_hat_j_i);
				}
			});

			// mul* H_hat_i proof
			let H_hat_i_randomness = crate::utilities::sample_relatively_prime_integer(
				&self.presigning_transcript.secrets.ek.n,
			);
			let H_hat_i = Paillier::encrypt_with_chosen_randomness(
				&self.presigning_transcript.secrets.ek,
				RawPlaintext::from(
					self.presigning_transcript.k_i.mul(&self.presigning_transcript.secrets.x_i),
				),
				&Randomness::from(H_hat_i_randomness),
			)
			.into();
			let witness_H_hat_i = PaillierMultiplicationVersusGroupWitness {
				x: self.presigning_transcript.secrets.x_i,
				rho: self.presigning_transcript.rho_i.mul(&H_hat_i_randomness),
				phantom: PhantomData,
			};

			let X_i = Point::<Secp256k1>::generator() *
				Scalar::from_bigint(&self.presigning_transcript.secrets.x_i);

			let proof_H_hat_i: HashMap<
				u16,
				PaillierMultiplicationVersusGroupProof<Secp256k1, Sha256>,
			> = HashMap::new();
			let statement_H_hat_i: HashMap<
				u16,
				PaillierMultiplicationVersusGroupStatement<Secp256k1, Sha256>,
			> = HashMap::new();

			self.ssid.P.par_iter().map(|l| {
				if *l != self.ssid.X.i {
					let statement_H_hat_l_i = PaillierMultiplicationVersusGroupStatement {
						N0: self.presigning_transcript.secrets.ek.n,
						NN0: self.presigning_transcript.secrets.ek.nn,
						C: self.presigning_transcript.K_i,
						D: H_hat_i,
						X: X_i,
						N_hat: *self.presigning_transcript.N_hats.get(l).unwrap_or(&BigInt::zero()),
						s: *self.presigning_transcript.S.get(l).unwrap_or(&BigInt::zero()),
						t: *self.presigning_transcript.T.get(l).unwrap_or(&BigInt::zero()),
						phantom: PhantomData,
					};

					statement_H_hat_i.insert(*l, statement_H_hat_l_i);

					proof_H_hat_i.insert(
						*l,
						PaillierMultiplicationVersusGroupProof::<Secp256k1, Sha256>::prove(
							&witness_H_hat_i,
							&statement_H_hat_l_i,
						),
					);
				}
			});

			// dec proof
			let ciphertext = H_hat_i;
			let ciphertext_randomness = H_hat_i_randomness;
			self.ssid.P.iter().map(|j| {
				if *j != self.ssid.X.i {
					ciphertext
						.mul(&self.presigning_transcript.D_hat_i.get(j).unwrap_or(&BigInt::zero()))
						.mul(self.presigning_transcript.F_hat_j.get(&self.ssid.X.i).unwrap());
					ciphertext_randomness
						.mul(&s_hat_j_i)
						.mul(&self.presigning_transcript.r_hat_i.get(j).unwrap_or(&BigInt::zero()));
				}
			});

			BigInt::mod_pow(&ciphertext, &self.r, &self.presigning_transcript.secrets.ek.nn);
			ciphertext.mul(&BigInt::mod_pow(
				&self.presigning_transcript.K_i,
				&self.m,
				&self.presigning_transcript.secrets.ek.nn,
			));
			BigInt::mod_pow(
				&ciphertext_randomness,
				&self.r,
				&self.presigning_transcript.secrets.ek.nn,
			);
			ciphertext_randomness.mul(&BigInt::mod_pow(
				&self.presigning_transcript.K_i,
				&self.m,
				&self.presigning_transcript.secrets.ek.nn,
			));

			let witness_sigma_i = PaillierDecryptionModQWitness {
				y: Paillier::decrypt(
					&self.presigning_transcript.secrets.dk,
					RawCiphertext::from(ciphertext),
				)
				.into(),
				rho: ciphertext_randomness,
				phantom: PhantomData,
			};

			// l to statement
			let statement_sigma_i: HashMap<
				u16,
				PaillierDecryptionModQStatement<Secp256k1, Sha256>,
			> = HashMap::new();

			// l to proof
			let proof_sigma_i: HashMap<u16, PaillierDecryptionModQProof<Secp256k1, Sha256>> =
				HashMap::new();

			self.ssid.P.par_iter().map(|l| {
				if *l != self.ssid.X.i {
					let statement_sigma_l_i = PaillierDecryptionModQStatement {
						S: *self.presigning_transcript.S.get(l).unwrap_or(&BigInt::zero()),
						T: *self.presigning_transcript.T.get(l).unwrap_or(&BigInt::zero()),
						N_hat: *self.presigning_transcript.N_hats.get(l).unwrap_or(&BigInt::zero()),
						N0: self.presigning_transcript.secrets.ek.n,
						NN0: self.presigning_transcript.secrets.ek.nn,
						C: ciphertext,
						x: self.sigma_i,
						ek_prover: self.presigning_transcript.secrets.ek,
						phantom: PhantomData,
					};

					statement_sigma_i.insert(*l, statement_sigma_l_i);

					proof_sigma_i.insert(
						*l,
						PaillierDecryptionModQProof::<Secp256k1, Sha256>::prove(
							&witness_sigma_i,
							&statement_sigma_l_i,
						),
					);
				}
			});

			let body = Some(SigningIdentifiableAbortMessage {
				i: self.ssid.X.i,
				proofs_D_hat_j_i,
				statements_D_hat_j_i,
				proof_H_hat_i,
				statement_H_hat_i,
				proof_sigma_i,
				statement_sigma_i,
			});
			output.push(Msg { sender: self.ssid.X.i, receiver: None, body });
			Ok(Round2 { ssid: self.ssid, output: None })
		}
	}

	pub fn is_expensive(&self) -> bool {
		false
	}

	pub fn expects_messages(i: u16, n: u16) -> Round0Messages {
		BroadcastMsgsStore::new(i, n)
	}
}

pub struct Round2 {
	ssid: SSID<Secp256k1>,
	output: Option<SigningOutput<Secp256k1>>,
}

impl Round2 {
	pub fn proceed(
		self,
		input: BroadcastMsgs<Option<SigningIdentifiableAbortMessage<Secp256k1>>>,
	) -> Result<Option<SigningOutput<Secp256k1>>> {
		if self.output.is_some() {
			Ok(Some(self.output.unwrap()))
		} else {
			for msg in input.into_vec() {
				let msg = msg.unwrap();
				// si stands for sender index
				let si = msg.i;
				// Check D_hat_i_j proofs
				self.ssid.P.par_iter().map(|j| {
					if *j != self.ssid.X.i {
						let D_hat_si_j_proof =
							msg.proofs_D_hat_j_i.get(&(self.ssid.X.i, *j)).unwrap();

						let statement_D_hat_si_j =
							msg.statements_D_hat_j_i.get(&(self.ssid.X.i, *j)).unwrap();

						if PaillierAffineOpWithGroupComInRangeProof::<Secp256k1, Sha256>::verify(
							D_hat_si_j_proof,
							statement_D_hat_si_j,
						)
						.is_err()
						{
							return Err(SignError::ProofVerificationError {
								proof_type: format!("aff-g"),
								proof_symbol: format!("D_hat_si_j"),
								verifying_party: self.ssid.X.i,
								faulty_party: *j,
							})
						}
					}
				});
				// Check H_j proofs
				let proof_H_hat_si = msg.proof_H_hat_i.get(&self.ssid.X.i).unwrap();
				let statement_H_hat_si = msg.statement_H_hat_i.get(&self.ssid.X.i).unwrap();

				if PaillierMultiplicationVersusGroupProof::verify(
					proof_H_hat_si,
					statement_H_hat_si,
				)
				.is_err()
				{
					return Err(SignError::ProofVerificationError {
						proof_type: format!("mul"),
						proof_symbol: format!("H_hat_si"),
						verifying_party: self.ssid.X.i,
						faulty_party: si,
					})
				}
				// Check delta_si_proof
				let proof_sigma_si = msg.proof_sigma_i.get(&self.ssid.X.i).unwrap();
				let statement_sigma_si = msg.statement_sigma_i.get(&self.ssid.X.i).unwrap();

				if PaillierDecryptionModQProof::verify(proof_sigma_si, statement_sigma_si).is_err()
				{
					return Err(SignError::ProofVerificationError {
						proof_type: format!("dec_q"),
						proof_symbol: format!("sigma_i"),
						verifying_party: self.ssid.X.i,
						faulty_party: si,
					})
				}
			}
			Ok(None)
		}
	}

	pub fn is_expensive(&self) -> bool {
		false
	}
	pub fn expects_messages(i: u16, n: u16) -> Round1Messages {
		BroadcastMsgsStore::new(i, n)
	}
}

type Result<T> = std::result::Result<T, SignError>;

#[derive(Error, Debug, Clone)]
pub enum SignError {
	#[error("Proof Verification Error")]
	ProofVerificationError(ErrorType),

	#[error("No Offline Stage Error")]
	NoOfflineStageError(ErrorType),
}
