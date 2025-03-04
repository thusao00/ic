use crate::DerivationPath;
use crate::*;
use ic_types::crypto::canister_threshold_sig::MasterEcdsaPublicKey;

// This is the conversion function used by ECDSA which returns the
// x-coordinate of a point reduced modulo the modulus of the scalar
// field.
pub(crate) fn ecdsa_conversion_function(pt: &EccPoint) -> ThresholdEcdsaResult<EccScalar> {
    let x = pt.affine_x()?;
    let x_bytes = x.as_bytes();
    EccScalar::from_bytes_wide(pt.curve_type(), &x_bytes)
}

fn convert_hash_to_integer(
    hashed_message: &[u8],
    curve_type: EccCurveType,
) -> ThresholdEcdsaResult<EccScalar> {
    // ECDSA has special rules for converting the hash to a scalar,
    // when the hash is larger than the curve order. If this check is
    // removed make sure these conversions are implemented, and not
    // just doing a reduction mod order using from_bytes_wide
    if hashed_message.len() != curve_type.scalar_bytes() {
        return Err(ThresholdEcdsaError::InvalidScalar);
    }

    // Even though the same size, the integer representation of the
    // message might be larger than the order, requiring a reduction.
    EccScalar::from_bytes_wide(curve_type, hashed_message)
}

fn derive_rho(
    curve_type: EccCurveType,
    hashed_message: &[u8],
    randomness: &Randomness,
    derivation_path: &DerivationPath,
    key_transcript: &IDkgTranscriptInternal,
    presig_transcript: &IDkgTranscriptInternal,
) -> ThresholdEcdsaResult<(EccScalar, EccScalar, EccScalar, EccPoint)> {
    let pre_sig = match &presig_transcript.combined_commitment {
        CombinedCommitment::ByInterpolation(PolynomialCommitment::Simple(c)) => c.constant_term(),
        _ => return Err(ThresholdEcdsaError::UnexpectedCommitmentType),
    };

    if pre_sig.curve_type() != curve_type {
        return Err(ThresholdEcdsaError::UnexpectedCommitmentType);
    }

    let (key_tweak, _chain_key) = derivation_path.derive_tweak(&key_transcript.constant_term())?;

    let mut ro = ro::RandomOracle::new("ic-crypto-tecdsa-rerandomize-presig");
    ro.add_bytestring("randomness", &randomness.get())?;
    ro.add_bytestring("hashed_message", hashed_message)?;
    ro.add_point("pre_sig", &pre_sig)?;
    ro.add_scalar("key_tweak", &key_tweak)?;
    let randomizer = ro.output_scalar(curve_type)?;

    // Rerandomize presignature
    let randomized_pre_sig =
        pre_sig.add_points(&EccPoint::generator_g(curve_type).scalar_mul(&randomizer)?)?;

    let rho = ecdsa_conversion_function(&randomized_pre_sig)?;

    Ok((rho, key_tweak, randomizer, randomized_pre_sig))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdEcdsaSigShareInternal {
    sigma_numerator: CommitmentOpening,
    sigma_denominator: CommitmentOpening,
}

impl ThresholdEcdsaSigShareInternal {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        derivation_path: &DerivationPath,
        hashed_message: &[u8],
        randomness: Randomness,
        key_transcript: &IDkgTranscriptInternal,
        presig_transcript: &IDkgTranscriptInternal,
        lambda: &CommitmentOpening,
        kappa_times_lambda: &CommitmentOpening,
        key_times_lambda: &CommitmentOpening,
        curve_type: EccCurveType,
    ) -> ThresholdEcdsaResult<Self> {
        let (rho, key_tweak, randomizer, _presig) = derive_rho(
            curve_type,
            hashed_message,
            &randomness,
            derivation_path,
            key_transcript,
            presig_transcript,
        )?;

        // Compute the message representative from the hash, which may require
        // a reduction if int(hashed_message) >= group_order
        let e = convert_hash_to_integer(hashed_message, curve_type)?;

        let theta = e.add(&rho.mul(&key_tweak)?)?;

        let (lambda_value, lambda_mask) = match lambda {
            CommitmentOpening::Pedersen(lambda_value, lambda_mask) => (lambda_value, lambda_mask),
            _ => return Err(ThresholdEcdsaError::UnexpectedCommitmentType),
        };

        // Compute shares of sigma's numerator, i.e. openings of
        // [nu] = theta*[lambda] + rho*[key_times_lambda]
        let nu = match key_times_lambda {
            CommitmentOpening::Pedersen(value, mask) => {
                let nu_value = theta.mul(lambda_value)?.add(&rho.mul(value)?)?;
                let nu_mask = theta.mul(lambda_mask)?.add(&rho.mul(mask)?)?;
                CommitmentOpening::Pedersen(nu_value, nu_mask)
            }
            _ => return Err(ThresholdEcdsaError::UnexpectedCommitmentType),
        };

        // Compute shares of sigma's denominator, i.e. openings of
        // [mu] = randomizer*[lambda] + [kappa_times_lambda]
        let mu = match kappa_times_lambda {
            CommitmentOpening::Pedersen(value, mask) => {
                let mu_value = randomizer.mul(lambda_value)?.add(value)?;
                let mu_mask = randomizer.mul(lambda_mask)?.add(mask)?;
                CommitmentOpening::Pedersen(mu_value, mu_mask)
            }
            _ => return Err(ThresholdEcdsaError::UnexpectedCommitmentType),
        };

        Ok(Self {
            sigma_numerator: nu,
            sigma_denominator: mu,
        })
    }

    /// Verify a signature share
    ///
    /// This function returns Ok(true) if the share seems completely valid,
    /// Ok(false) if the commitment values are incorrect, and some Err if the
    /// share is otherwise invalid, for instance because one of the values
    /// is a point for another elliptic curve, or if the wrong commitment
    /// type was included in a transcript.
    #[allow(clippy::many_single_char_names, clippy::too_many_arguments)]
    pub fn verify(
        &self,
        derivation_path: &DerivationPath,
        hashed_message: &[u8],
        randomness: Randomness,
        signer_index: NodeIndex,
        key_transcript: &IDkgTranscriptInternal,
        presig_transcript: &IDkgTranscriptInternal,
        lambda: &IDkgTranscriptInternal,
        kappa_times_lambda: &IDkgTranscriptInternal,
        key_times_lambda: &IDkgTranscriptInternal,
        curve_type: EccCurveType,
    ) -> ThresholdEcdsaResult<()> {
        // Compute rho and tweak
        let (rho, key_tweak, randomizer, _presig) = derive_rho(
            curve_type,
            hashed_message,
            &randomness,
            derivation_path,
            key_transcript,
            presig_transcript,
        )?;

        // Compute theta
        let e = convert_hash_to_integer(hashed_message, curve_type)?;

        let theta = e.add(&rho.mul(&key_tweak)?)?;

        // Evaluate commitments at the receiver index
        let lambda_j = lambda.evaluate_at(signer_index)?;
        let kappa_times_lambda_j = kappa_times_lambda.evaluate_at(signer_index)?;
        let key_times_lambda_j = key_times_lambda.evaluate_at(signer_index)?;

        let sigma_num = lambda_j
            .scalar_mul(&theta)?
            .add_points(&key_times_lambda_j.scalar_mul(&rho)?)?;

        let sigma_den = lambda_j
            .scalar_mul(&randomizer)?
            .add_points(&kappa_times_lambda_j)?;

        match &self.sigma_numerator {
            CommitmentOpening::Pedersen(v, m) => {
                if sigma_num != EccPoint::pedersen(v, m)? {
                    return Err(ThresholdEcdsaError::InvalidCommitment);
                }
            }
            _ => return Err(ThresholdEcdsaError::UnexpectedCommitmentType),
        }

        match &self.sigma_denominator {
            CommitmentOpening::Pedersen(v, m) => {
                if sigma_den != EccPoint::pedersen(v, m)? {
                    return Err(ThresholdEcdsaError::InvalidCommitment);
                }
            }
            _ => return Err(ThresholdEcdsaError::UnexpectedCommitmentType),
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ThresholdEcdsaCombinedSigInternal {
    r: EccScalar,
    s: EccScalar,
}

impl ThresholdEcdsaCombinedSigInternal {
    pub fn serialize(&self) -> Vec<u8> {
        // EccScalar::serialize uses fixed length encoding
        let r_bytes = self.r.serialize();
        let s_bytes = self.s.serialize();

        let mut sig = Vec::with_capacity(r_bytes.len() + s_bytes.len());
        sig.extend_from_slice(&r_bytes);
        sig.extend_from_slice(&s_bytes);
        sig
    }

    pub fn deserialize(
        algorithm_id: AlgorithmId,
        bytes: &[u8],
    ) -> ThresholdEcdsaSerializationResult<Self> {
        let curve_type = match algorithm_id {
            AlgorithmId::ThresholdEcdsaSecp256k1 => Ok(EccCurveType::K256),
            x => Err(ThresholdEcdsaSerializationError(format!(
                "Invalid algorithm {:?} for threshold ECDSA",
                x
            ))),
        }?;

        let slen = curve_type.scalar_bytes();

        if bytes.len() != 2 * slen {
            return Err(ThresholdEcdsaSerializationError(
                "Bad signature length".to_string(),
            ));
        }

        let r = EccScalar::deserialize(curve_type, &bytes[..slen])
            .map_err(|e| ThresholdEcdsaSerializationError(format!("Invalid r: {:?}", e)))?;

        let s = EccScalar::deserialize(curve_type, &bytes[slen..])
            .map_err(|e| ThresholdEcdsaSerializationError(format!("Invalid s: {:?}", e)))?;

        Ok(Self { r, s })
    }
}

impl ThresholdEcdsaCombinedSigInternal {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        derivation_path: &DerivationPath,
        hashed_message: &[u8],
        randomness: Randomness,
        key_transcript: &IDkgTranscriptInternal,
        presig_transcript: &IDkgTranscriptInternal,
        reconstruction_threshold: NumberOfNodes,
        sig_shares: &BTreeMap<NodeIndex, ThresholdEcdsaSigShareInternal>,
        curve_type: EccCurveType,
    ) -> ThresholdEcdsaResult<Self> {
        let reconstruction_threshold = reconstruction_threshold.get() as usize;
        if sig_shares.len() < reconstruction_threshold {
            return Err(ThresholdEcdsaError::InsufficientDealings);
        }

        let (rho, _key_tweak, _randomizer, _presig) = derive_rho(
            curve_type,
            hashed_message,
            &randomness,
            derivation_path,
            key_transcript,
            presig_transcript,
        )?;

        // Compute sigma's numerator via interpolation
        let mut x_values = Vec::with_capacity(reconstruction_threshold);
        let mut numerator_samples = Vec::with_capacity(reconstruction_threshold);
        let mut denominator_samples = Vec::with_capacity(reconstruction_threshold);

        for (index, sig_share) in sig_shares.iter().take(reconstruction_threshold) {
            x_values.push(*index);
            // Reconstruction of the signature share does not require recombining the
            // masking values.
            if let CommitmentOpening::Pedersen(c, _) = &sig_share.sigma_numerator {
                numerator_samples.push(c.clone());
            } else {
                return Err(ThresholdEcdsaError::UnexpectedCommitmentType);
            }

            if let CommitmentOpening::Pedersen(c, _) = &sig_share.sigma_denominator {
                denominator_samples.push(c.clone());
            } else {
                return Err(ThresholdEcdsaError::UnexpectedCommitmentType);
            }
        }

        let coefficients = LagrangeCoefficients::at_zero(curve_type, &x_values)?;
        let numerator = coefficients.interpolate_scalar(&numerator_samples)?;
        let denominator = coefficients.interpolate_scalar(&denominator_samples)?;

        let denominator_inv = match denominator.invert() {
            Some(s) => s,
            None => return Err(ThresholdEcdsaError::InterpolationError),
        };

        let sigma = numerator.mul(&denominator_inv)?;

        // Always use the smaller value of s
        let norm_sigma = if sigma.is_high() {
            sigma.negate()
        } else {
            sigma
        };

        Ok(Self {
            r: rho,
            s: norm_sigma,
        })
    }

    /// Verify a threshold ECDSA signature
    ///
    /// This not only verifies the basic signature equation but also that
    /// it was generated with a particular presignature transcript.
    ///
    /// It also verifies that s is normalized to be in [0,n/2) following
    /// the malleability prevention approach of BTC/ETH.
    ///
    /// This function returns Ok(true) if the signature seems completely
    /// valid, Ok(false) if something was wrong the the signature itself
    /// (wrong rho, `s` too large, or the ECDSA equation fails to verify),
    /// or some Err if the signature or parameters are otherwise invalid,
    /// for instance because one of the values is on the wrong curve.
    pub fn verify(
        &self,
        derivation_path: &DerivationPath,
        hashed_message: &[u8],
        randomness: Randomness,
        presig_transcript: &IDkgTranscriptInternal,
        key_transcript: &IDkgTranscriptInternal,
        curve_type: EccCurveType,
    ) -> ThresholdEcdsaResult<()> {
        if self.r.is_zero() || self.s.is_zero() {
            return Err(ThresholdEcdsaError::InvalidSignature);
        }

        let msg = convert_hash_to_integer(hashed_message, curve_type)?;

        let (rho, key_tweak, _, pre_sig) = derive_rho(
            curve_type,
            hashed_message,
            &randomness,
            derivation_path,
            key_transcript,
            presig_transcript,
        )?;

        if self.r != rho {
            return Err(ThresholdEcdsaError::InvalidSignature);
        }

        // We require s normalization for all curves
        if self.s.is_high() {
            return Err(ThresholdEcdsaError::InvalidSignature);
        }

        let master_public_key = key_transcript.constant_term();
        let tweak_g = EccPoint::mul_by_g(&key_tweak)?;
        let public_key = tweak_g.add_points(&master_public_key)?;

        // This return shouldn't happen because we already checked that s != 0 above
        let s_inv = match self.s.invert() {
            Some(si) => si,
            None => return Err(ThresholdEcdsaError::InvalidSignature),
        };

        let u1 = msg.mul(&s_inv)?;
        let u2 = self.r.mul(&s_inv)?;

        let rp = EccPoint::mul_2_points(&EccPoint::generator_g(curve_type), &u1, &public_key, &u2)?;

        if rp.is_infinity()? {
            return Err(ThresholdEcdsaError::InvalidSignature);
        }

        /*
        In normal ECDSA verification we would have

        r = x_coordinate(k*G) % order

        and during verification check

        r == x_coordinate(rp) % order

        To aid the security proof, instead here we use pre_sig (which equals k*G)
        and check that x_coordinate(pre_sig) == x_coordinate(rp)

        Due to normalization of s pre_sig and rp may differ in their sign, so
        we only check the x coordinate.
        */

        if rp.affine_x()? != pre_sig.affine_x()? {
            return Err(ThresholdEcdsaError::InvalidSignature);
        }

        // accept:
        Ok(())
    }
}

/// Returns a public key derived from `master_public_key` according to the
/// `derivation_path`.  The algorithm id of the derived key is the same
/// as the algorithm id of `master_public_key`.
pub fn derive_public_key(
    master_public_key: &MasterEcdsaPublicKey,
    derivation_path: &DerivationPath,
) -> ThresholdEcdsaResult<EcdsaPublicKey> {
    let raw_master_pk = match master_public_key.algorithm_id {
        AlgorithmId::EcdsaSecp256k1 => {
            EccPoint::deserialize(EccCurveType::K256, &master_public_key.public_key)?
        }
        _ => return Err(ThresholdEcdsaError::CurveMismatch),
    };
    // Compute tweak
    let (key_tweak, chain_key) = derivation_path.derive_tweak(&raw_master_pk)?;
    let tweak_g = EccPoint::mul_by_g(&key_tweak)?;
    let public_key = tweak_g.add_points(&raw_master_pk)?;

    Ok(EcdsaPublicKey {
        algorithm_id: master_public_key.algorithm_id,
        public_key: public_key.serialize(),
        chain_key,
    })
}
