use core::fmt::Debug;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use crate::{
    common_utils::{reverse_bit_order, reverse_bits_limited},
    eip_4844::{
        blob_to_polynomial, compute_powers, hash, hash_to_bls_field, BYTES_PER_COMMITMENT,
        BYTES_PER_FIELD_ELEMENT, BYTES_PER_PROOF,
    },
    eth::CELLS_PER_EXT_BLOB,
    FFTFr, FFTSettings, Fr, G1Affine, G1Fp, G1LinComb, KZGSettings, PairingVerify, Poly, FFTG1, G1,
    G2,
};

pub const RANDOM_CHALLENGE_KZG_CELL_BATCH_DOMAIN: [u8; 16] = *b"RCKZGCBATCH__V1_";

macro_rules! cfg_iter_mut {
    ($collection:expr) => {{
        #[cfg(feature = "parallel")]
        {
            $collection.par_iter_mut()
        }
        #[cfg(not(feature = "parallel"))]
        {
            $collection.iter_mut()
        }
    }};
}

macro_rules! cfg_iter {
    ($collection:expr) => {{
        #[cfg(feature = "parallel")]
        {
            $collection.par_iter()
        }
        #[cfg(not(feature = "parallel"))]
        {
            $collection.iter()
        }
    }};
}

pub trait EcBackend {
    type Fr: Fr + Debug + Send;
    type G1Fp: G1Fp;
    type G1Affine: G1Affine<Self::G1, Self::G1Fp>;
    type G1: G1
        + G1LinComb<Self::Fr, Self::G1Fp, Self::G1Affine>
        + PairingVerify<Self::G1, Self::G2>;
    type G2: G2;
    type Poly: Poly<Self::Fr>;
    type FFTSettings: FFTSettings<Self::Fr> + FFTFr<Self::Fr> + FFTG1<Self::G1>;
    type KZGSettings: KZGSettings<
        Self::Fr,
        Self::G1,
        Self::G2,
        Self::FFTSettings,
        Self::Poly,
        Self::G1Fp,
        Self::G1Affine,
    >;
}

pub trait Preset {
    const FIELD_ELEMENTS_PER_BLOB: usize;
    const FIELD_ELEMENTS_PER_EXT_BLOB: usize;
    const CELLS_PER_EXT_BLOB: usize;
}

fn deduplicate_commitments<TG1: PartialEq + Clone>(
    commitments: &mut [TG1],
    indicies: &mut [usize],
    count: &mut usize,
) {
    if *count == 0 {
        return;
    }

    indicies[0] = 0;
    let mut new_count = 1;

    for i in 1..*count {
        let mut exist = false;
        for j in 0..new_count {
            if commitments[i] == commitments[j] {
                indicies[i] = j;
                exist = true;
                break;
            }
        }

        if !exist {
            commitments[new_count] = commitments[i].clone();
            indicies[i] = new_count;
            new_count += 1;
        }
    }
}

/**
 * This is a precomputed map of cell index to reverse-bits-limited cell index.
 *
 * for (size_t i = 0; i < CELLS_PER_EXT_BLOB; i++)
 *   printf("%#04llx,\n", reverse_bits_limited(CELLS_PER_EXT_BLOB, i));
 *
 * Because of the way our evaluation domain is defined, we can use CELL_INDICES_RBL to find the
 * coset factor of a cell. In particular, for cell i, its coset factor is
 * roots_of_unity[CELLS_INDICES_RBL[i]].
 */
const CELL_INDICES_RBL: [usize; 128] = [
    0x00, 0x40, 0x20, 0x60, 0x10, 0x50, 0x30, 0x70, 0x08, 0x48, 0x28, 0x68, 0x18, 0x58, 0x38, 0x78,
    0x04, 0x44, 0x24, 0x64, 0x14, 0x54, 0x34, 0x74, 0x0c, 0x4c, 0x2c, 0x6c, 0x1c, 0x5c, 0x3c, 0x7c,
    0x02, 0x42, 0x22, 0x62, 0x12, 0x52, 0x32, 0x72, 0x0a, 0x4a, 0x2a, 0x6a, 0x1a, 0x5a, 0x3a, 0x7a,
    0x06, 0x46, 0x26, 0x66, 0x16, 0x56, 0x36, 0x76, 0x0e, 0x4e, 0x2e, 0x6e, 0x1e, 0x5e, 0x3e, 0x7e,
    0x01, 0x41, 0x21, 0x61, 0x11, 0x51, 0x31, 0x71, 0x09, 0x49, 0x29, 0x69, 0x19, 0x59, 0x39, 0x79,
    0x05, 0x45, 0x25, 0x65, 0x15, 0x55, 0x35, 0x75, 0x0d, 0x4d, 0x2d, 0x6d, 0x1d, 0x5d, 0x3d, 0x7d,
    0x03, 0x43, 0x23, 0x63, 0x13, 0x53, 0x33, 0x73, 0x0b, 0x4b, 0x2b, 0x6b, 0x1b, 0x5b, 0x3b, 0x7b,
    0x07, 0x47, 0x27, 0x67, 0x17, 0x57, 0x37, 0x77, 0x0f, 0x4f, 0x2f, 0x6f, 0x1f, 0x5f, 0x3f, 0x7f,
];

pub trait DAS<B: EcBackend, const FIELD_ELEMENTS_PER_CELL: usize, P: Preset> {
    fn kzg_settings(&self) -> &B::KZGSettings;

    fn recover_cells_and_kzg_proofs(
        &self,
        recovered_cells: &mut [[B::Fr; FIELD_ELEMENTS_PER_CELL]],
        recovered_proofs: Option<&mut [B::G1]>,
        cell_indices: &[usize],
        cells: &[[B::Fr; FIELD_ELEMENTS_PER_CELL]],
    ) -> Result<(), String> {
        if recovered_cells.len() != P::CELLS_PER_EXT_BLOB
            || recovered_proofs
                .as_ref()
                .is_some_and(|it| it.len() != P::CELLS_PER_EXT_BLOB)
        {
            return Err("Invalid output array length".to_string());
        }

        if cells.len() != cell_indices.len() {
            return Err(
                "Cell indicies mismatch - cells length must be equal to cell indicies length"
                    .to_string(),
            );
        }

        if cells.len() > P::CELLS_PER_EXT_BLOB {
            return Err("Cell length cannot be larger than CELLS_PER_EXT_BLOB".to_string());
        }

        if cells.len() < P::CELLS_PER_EXT_BLOB / 2 {
            return Err(
                "Impossible to recover - cells length cannot be less than CELLS_PER_EXT_BLOB / 2"
                    .to_string(),
            );
        }

        for cell_index in cell_indices {
            if *cell_index >= P::CELLS_PER_EXT_BLOB {
                return Err("Cell index cannot be larger than CELLS_PER_EXT_BLOB".to_string());
            }
        }

        for cell in recovered_cells.iter_mut() {
            for fr in cell {
                *fr = B::Fr::null();
            }
        }

        for i in 0..cells.len() {
            let index = cell_indices[i];

            if recovered_cells[index]
                .as_ref()
                .iter()
                .any(|cell| !cell.is_null())
            {
                return Err("Invalid output cell".to_string());
            }

            recovered_cells[index] = cells[i].clone();
        }

        let fft_settings = self.kzg_settings().get_fft_settings();

        if cells.len() != P::CELLS_PER_EXT_BLOB {
            recover_cells::<FIELD_ELEMENTS_PER_CELL, B, P>(
                recovered_cells.as_flattened_mut(),
                cell_indices,
                fft_settings,
            )?;
        }

        #[allow(clippy::redundant_slicing)]
        let recovered_cells = &recovered_cells[..];

        if let Some(recovered_proofs) = recovered_proofs {
            let mut poly = vec![B::Fr::default(); P::FIELD_ELEMENTS_PER_EXT_BLOB];
            poly.clone_from_slice(recovered_cells.as_flattened());
            poly_lagrange_to_monomial::<B>(&mut poly, fft_settings)?;

            let res = compute_fk20_proofs::<FIELD_ELEMENTS_PER_CELL, B>(
                &poly,
                P::FIELD_ELEMENTS_PER_BLOB,
                fft_settings,
                self.kzg_settings(),
            )?;
            recovered_proofs.clone_from_slice(&res);

            reverse_bit_order(recovered_proofs)?;
        }

        Ok(())
    }

    fn compute_cells_and_kzg_proofs(
        &self,
        cells: Option<&mut [[B::Fr; FIELD_ELEMENTS_PER_CELL]]>,
        proofs: Option<&mut [B::G1]>,
        blob: &[B::Fr],
    ) -> Result<(), String> {
        if cells.is_none() && proofs.is_none() {
            return Err("Both cells & proofs cannot be none".to_string());
        }

        let poly = blob_to_polynomial::<B::Fr, B::Poly>(blob)?;

        let mut poly_monomial = vec![B::Fr::zero(); P::FIELD_ELEMENTS_PER_EXT_BLOB];
        poly_monomial[0..P::FIELD_ELEMENTS_PER_BLOB].clone_from_slice(poly.get_coeffs());

        let fft_settings = self.kzg_settings().get_fft_settings();
        poly_lagrange_to_monomial::<B>(
            &mut poly_monomial[..P::FIELD_ELEMENTS_PER_BLOB],
            fft_settings,
        )?;

        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;

            if let Some(cells) = cells {
                let fft_result = fft_settings.fft_fr(&poly_monomial, false)?;

                let flattened_cells = fft_result.as_slice().to_vec();
                let num_cells = cells.len();
                flattened_cells
                    .par_chunks(flattened_cells.len() / num_cells)
                    .zip(cells.par_iter_mut())
                    .for_each(|(chunk, cell)| {
                        cell.clone_from_slice(chunk);
                    });

                reverse_bit_order(cells.as_flattened_mut())?;
            }

            if let Some(proofs) = proofs {
                let fk20_proofs = compute_fk20_proofs::<FIELD_ELEMENTS_PER_CELL, B>(
                    &poly_monomial,
                    P::FIELD_ELEMENTS_PER_BLOB,
                    fft_settings,
                    self.kzg_settings(),
                )?;

                proofs.par_iter_mut().zip(fk20_proofs.par_iter()).for_each(
                    |(proof, result_proof)| {
                        *proof = *result_proof;
                    },
                );

                reverse_bit_order(proofs)?;
            }
        }

        #[cfg(not(feature = "parallel"))]
        {
            // Compute cells sequentially
            if let Some(cells) = cells {
                cells
                    .as_flattened_mut()
                    .clone_from_slice(&fft_settings.fft_fr(&poly_monomial, false)?);

                reverse_bit_order(cells.as_flattened_mut())?;
            }

            // Compute proofs sequentially
            if let Some(proofs) = proofs {
                let result = compute_fk20_proofs::<FIELD_ELEMENTS_PER_CELL, B>(
                    &poly_monomial,
                    P::FIELD_ELEMENTS_PER_BLOB,
                    fft_settings,
                    self.kzg_settings(),
                )?;
                proofs.clone_from_slice(&result);
                reverse_bit_order(proofs)?;
            }
        }

        Ok(())
    }

    fn verify_cell_kzg_proof_batch(
        &self,
        commitments: &[B::G1],
        cell_indices: &[usize],
        cells: &[[B::Fr; FIELD_ELEMENTS_PER_CELL]],
        proofs: &[B::G1],
    ) -> Result<bool, String> {
        if cells.len() != cell_indices.len() {
            return Err("Cell count mismatch".to_string());
        }

        if commitments.len() != cells.len() {
            return Err("Commitment count mismatch".to_string());
        }

        if proofs.len() != cells.len() {
            return Err("Proof count mismatch".to_string());
        }

        if cells.is_empty() {
            return Ok(true);
        }

        if cfg_iter!(cell_indices).any(|&cell_index| cell_index >= CELLS_PER_EXT_BLOB) {
            return Err("Invalid cell index".to_string());
        }

        if cfg_iter!(proofs).any(|proof| !proof.is_valid()) {
            return Err("Proof is not valid".to_string());
        }

        let mut new_count = commitments.len();
        let mut unique_commitments = commitments.to_vec();
        let mut commitment_indices = vec![0usize; cells.len()];
        deduplicate_commitments(
            &mut unique_commitments,
            &mut commitment_indices,
            &mut new_count,
        );

        if cfg_iter!(unique_commitments).any(|commitment| !commitment.is_valid()) {
            return Err("Commitment is not valid".to_string());
        }

        let fft_settings = self.kzg_settings().get_fft_settings();

        let unique_commitments = &unique_commitments[0..new_count];

        let r_powers =
            compute_r_powers_for_verify_cell_kzg_proof_batch::<FIELD_ELEMENTS_PER_CELL, B>(
                unique_commitments,
                &commitment_indices,
                cell_indices,
                cells,
                proofs,
            )?;

        let proof_lincomb = B::G1::g1_lincomb(proofs, &r_powers, cells.len(), None);

        let final_g1_sum = compute_weighted_sum_of_commitments::<B>(
            unique_commitments,
            &commitment_indices,
            &r_powers,
        );

        let interpolation_poly_commit =
            compute_commitment_to_aggregated_interpolation_poly::<FIELD_ELEMENTS_PER_CELL, B, P>(
                &r_powers,
                cell_indices,
                cells,
                fft_settings,
                self.kzg_settings().get_g1_monomial(),
            )?;

        let final_g1_sum = final_g1_sum.sub(&interpolation_poly_commit);

        let weighted_sum_of_proofs = computed_weighted_sum_of_proofs::<
            FIELD_ELEMENTS_PER_CELL,
            B,
            P,
        >(proofs, &r_powers, cell_indices, fft_settings)?;

        let final_g1_sum = final_g1_sum.add(&weighted_sum_of_proofs);

        let power_of_s = &self.kzg_settings().get_g2_monomial()[FIELD_ELEMENTS_PER_CELL];

        Ok(B::G1::verify(
            &final_g1_sum,
            &B::G2::generator(),
            &proof_lincomb,
            power_of_s,
        ))
    }
}

fn shift_poly<B: EcBackend>(poly: &mut [B::Fr], shift_factor: &B::Fr) {
    let mut factor_power = B::Fr::one();
    for coeff in poly.iter_mut().skip(1) {
        factor_power = factor_power.mul(shift_factor);
        *coeff = coeff.mul(&factor_power);
    }
}

fn coset_fft<B: EcBackend>(
    mut input: Vec<B::Fr>,
    fft_settings: &B::FFTSettings,
) -> Result<Vec<B::Fr>, String> {
    if input.is_empty() {
        return Err("Invalid input length".to_string());
    }

    // TODO: move 7 to constant
    shift_poly::<B>(&mut input, &B::Fr::from_u64(7));

    fft_settings.fft_fr(&input, false)
}

fn coset_ifft<B: EcBackend>(
    input: &[B::Fr],
    fft_settings: &B::FFTSettings,
) -> Result<Vec<B::Fr>, String> {
    if input.is_empty() {
        return Err("Invalid input length".to_string());
    }

    let mut output = fft_settings.fft_fr(input, true)?;

    // TODO: move 1/7 to constant
    shift_poly::<B>(&mut output, &B::Fr::one().div(&B::Fr::from_u64(7))?);

    Ok(output)
}

fn compute_vanishing_polynomial_from_roots<B: EcBackend>(
    roots: &[B::Fr],
) -> Result<Vec<B::Fr>, String> {
    if roots.is_empty() {
        return Err("Roots cannot be empty".to_string());
    }

    let mut poly = Vec::new();
    poly.push(roots[0].negate());

    for i in 1..roots.len() {
        let neg_root = roots[i].negate();

        poly.push(neg_root.add(&poly[i - 1]));

        for j in (1..i).rev() {
            poly[j] = poly[j].mul(&neg_root).add(&poly[j - 1]);
        }
        poly[0] = poly[0].mul(&neg_root);
    }

    poly.push(B::Fr::one());

    Ok(poly)
}

fn vanishing_polynomial_for_missing_cells<
    const FIELD_ELEMENTS_PER_CELL: usize,
    B: EcBackend,
    P: Preset,
>(
    missing_cell_indicies: &[usize],
    fft_settings: &B::FFTSettings,
) -> Result<Vec<B::Fr>, String> {
    if missing_cell_indicies.is_empty() || missing_cell_indicies.len() >= P::CELLS_PER_EXT_BLOB {
        return Err("Invalid missing cell indicies count".to_string());
    }

    let stride = P::FIELD_ELEMENTS_PER_EXT_BLOB / P::CELLS_PER_EXT_BLOB;

    let roots = missing_cell_indicies
        .iter()
        .map(|i| fft_settings.get_roots_of_unity_at(*i * stride))
        .collect::<Vec<_>>();

    let short_vanishing_poly = compute_vanishing_polynomial_from_roots::<B>(&roots)?;

    let mut vanishing_poly = vec![B::Fr::zero(); P::FIELD_ELEMENTS_PER_EXT_BLOB];

    for (i, coeff) in short_vanishing_poly.into_iter().enumerate() {
        vanishing_poly[i * FIELD_ELEMENTS_PER_CELL] = coeff
    }

    Ok(vanishing_poly)
}

fn recover_cells<const FIELD_ELEMENTS_PER_CELL: usize, B: EcBackend, P: Preset>(
    output: &mut [B::Fr],
    cell_indicies: &[usize],
    fft_settings: &B::FFTSettings,
) -> Result<(), String> {
    let mut missing_cell_indicies = Vec::new();

    let mut cells_brp = output.to_vec();
    reverse_bit_order(&mut cells_brp)?;

    for i in 0..P::CELLS_PER_EXT_BLOB {
        if !cell_indicies.contains(&i) {
            missing_cell_indicies.push(reverse_bits_limited(P::CELLS_PER_EXT_BLOB, i));
        }
    }

    let missing_cell_indicies = &missing_cell_indicies[..];

    if missing_cell_indicies.len() > P::CELLS_PER_EXT_BLOB / 2 {
        return Err("Not enough cells".to_string());
    }

    let vanishing_poly_coeff = vanishing_polynomial_for_missing_cells::<
        FIELD_ELEMENTS_PER_CELL,
        B,
        P,
    >(missing_cell_indicies, fft_settings)?;

    let vanishing_poly_eval = fft_settings.fft_fr(&vanishing_poly_coeff, false)?;

    let mut extended_evaluation_times_zero = Vec::with_capacity(P::FIELD_ELEMENTS_PER_EXT_BLOB);

    for i in 0..P::FIELD_ELEMENTS_PER_EXT_BLOB {
        if cells_brp[i].is_null() {
            extended_evaluation_times_zero.push(B::Fr::zero());
        } else {
            extended_evaluation_times_zero.push(cells_brp[i].mul(&vanishing_poly_eval[i]));
        }
    }

    let extended_evaluation_times_zero_coeffs =
        fft_settings.fft_fr(&extended_evaluation_times_zero, true)?;
    let mut extended_evaluations_over_coset =
        coset_fft::<B>(extended_evaluation_times_zero_coeffs, fft_settings)?;

    let vanishing_poly_over_coset = coset_fft::<B>(vanishing_poly_coeff, fft_settings)?;

    for i in 0..P::FIELD_ELEMENTS_PER_EXT_BLOB {
        extended_evaluations_over_coset[i] =
            extended_evaluations_over_coset[i].div(&vanishing_poly_over_coset[i])?;
    }

    let reconstructed_poly_coeff = coset_ifft::<B>(&extended_evaluations_over_coset, fft_settings)?;

    let out = fft_settings.fft_fr(&reconstructed_poly_coeff, false)?;
    output.clone_from_slice(&out);

    reverse_bit_order(output)?;

    Ok(())
}

fn poly_lagrange_to_monomial<B: EcBackend>(
    lagrange_poly: &mut [B::Fr],
    fft_settings: &B::FFTSettings,
) -> Result<(), String> {
    let mut poly = lagrange_poly.to_vec();

    reverse_bit_order(&mut poly)?;

    lagrange_poly.clone_from_slice(&fft_settings.fft_fr(&poly, true)?);

    Ok(())
}

fn toeplitz_coeffs_stride<B: EcBackend>(
    out: &mut [B::Fr],
    input: &[B::Fr],
    n: usize,
    offset: usize,
    stride: usize,
) -> Result<(), String> {
    if stride == 0 {
        return Err("Stride cannot be zero".to_string());
    }

    let k = n / stride;
    let k2 = k * 2;

    out[0] = input[n - 1 - offset].clone();
    {
        let mut i = 1;
        while i <= k + 1 && i < k2 {
            out[i] = B::Fr::zero();
            i += 1;
        }
    };

    {
        let mut i = k + 2;
        let mut j = 2 * stride - offset - 1;
        while i < k2 {
            out[i] = input[j].clone();
            i += 1;
            j += stride;
        }
    };

    Ok(())
}

fn compute_fk20_proofs<const FIELD_ELEMENTS_PER_CELL: usize, B: EcBackend>(
    poly: &[B::Fr],
    n: usize,
    fft_settings: &B::FFTSettings,
    kzg_settings: &B::KZGSettings,
) -> Result<Vec<B::G1>, String> {
    let k = n / FIELD_ELEMENTS_PER_CELL;
    let k2 = k * 2;

    let mut coeffs = vec![vec![B::Fr::default(); k]; k2];
    let mut h_ext_fft = vec![B::G1::identity(); k2];
    let mut toeplitz_coeffs = vec![B::Fr::default(); k2];
    let mut toeplitz_coeffs_fft = vec![B::Fr::default(); k2];

    for i in 0..FIELD_ELEMENTS_PER_CELL {
        toeplitz_coeffs_stride::<B>(&mut toeplitz_coeffs, poly, n, i, FIELD_ELEMENTS_PER_CELL)?;
        toeplitz_coeffs_fft.clone_from_slice(&fft_settings.fft_fr(&toeplitz_coeffs, false)?);
        for j in 0..k2 {
            coeffs[j][i] = toeplitz_coeffs_fft[j].clone();
        }
    }

    for i in 0..k2 {
        h_ext_fft[i] = B::G1::g1_lincomb(
            kzg_settings.get_x_ext_fft_column(i),
            &coeffs[i],
            FIELD_ELEMENTS_PER_CELL,
            None,
        );
    }

    let mut h = fft_settings.fft_g1(&h_ext_fft, true)?;

    cfg_iter_mut!(h)
        .take(k2)
        .skip(k)
        .for_each(|h| *h = B::G1::identity());

    fft_settings.fft_g1(&h, false)
}

fn compute_r_powers_for_verify_cell_kzg_proof_batch<
    const FIELD_ELEMENTS_PER_CELL: usize,
    B: EcBackend,
>(
    commitments: &[B::G1],
    commitment_indices: &[usize],
    cell_indices: &[usize],
    cells: &[[B::Fr; FIELD_ELEMENTS_PER_CELL]],
    proofs: &[B::G1],
) -> Result<Vec<B::Fr>, String> {
    if commitment_indices.len() != cells.len()
        || cell_indices.len() != cells.len()
        || proofs.len() != cells.len()
    {
        return Err("Cell count mismatch".to_string());
    }

    // TODO: challenge generation probably also has to be in preset

    let input_size = RANDOM_CHALLENGE_KZG_CELL_BATCH_DOMAIN.len()
        + size_of::<u64>()
        + size_of::<u64>()
        + size_of::<u64>()
        // probably, BYTES_PER_COMMITMENT should be in backend trait - 
        // currently impossible due to encoded commitment length in G1 trait
        + (commitments.len() * BYTES_PER_COMMITMENT)
        + (cells.len() * size_of::<u64>())
        + (cells.len() * size_of::<u64>())
        + (cells.len() * (FIELD_ELEMENTS_PER_CELL * BYTES_PER_FIELD_ELEMENT))
        + (cells.len() * BYTES_PER_PROOF);

    let mut bytes = vec![0; input_size];
    bytes[..16].copy_from_slice(&RANDOM_CHALLENGE_KZG_CELL_BATCH_DOMAIN);
    bytes[16..24].copy_from_slice(&(FIELD_ELEMENTS_PER_CELL as u64).to_be_bytes());
    bytes[24..32].copy_from_slice(&(commitments.len() as u64).to_be_bytes());
    bytes[32..40].copy_from_slice(&(cells.len() as u64).to_be_bytes());

    let mut offset = 40;
    for commitment in commitments {
        bytes[offset..(offset + BYTES_PER_COMMITMENT)].copy_from_slice(&commitment.to_bytes());
        offset += BYTES_PER_COMMITMENT;
    }

    for i in 0..cells.len() {
        bytes[offset..(offset + 8)].copy_from_slice(&(commitment_indices[i] as u64).to_be_bytes());
        offset += 8;

        bytes[offset..(offset + 8)].copy_from_slice(&(cell_indices[i] as u64).to_be_bytes());
        offset += 8;

        bytes[offset..(offset + (FIELD_ELEMENTS_PER_CELL * BYTES_PER_FIELD_ELEMENT))]
            .copy_from_slice(
                &cells[i]
                    .as_ref()
                    .iter()
                    .flat_map(|fr| fr.to_bytes())
                    .collect::<Vec<_>>(),
            );
        offset += FIELD_ELEMENTS_PER_CELL * BYTES_PER_FIELD_ELEMENT;

        bytes[offset..(offset + BYTES_PER_PROOF)].copy_from_slice(&(proofs[i].to_bytes()));
        offset += BYTES_PER_PROOF;
    }

    let bytes = &bytes[..];

    if offset != input_size {
        return Err("Failed to create challenge - invalid length".to_string());
    }

    // hash function (as well as whole algo above I guess?) should be in Preset (or backend, not clear for now)
    let eval_challenge = hash(bytes);
    let r = hash_to_bls_field(&eval_challenge);

    Ok(compute_powers(&r, cells.len()))
}

fn compute_weighted_sum_of_commitments<B: EcBackend>(
    commitments: &[B::G1],
    commitment_indices: &[usize],
    r_powers: &[B::Fr],
) -> B::G1 {
    let mut commitment_weights = vec![B::Fr::zero(); commitments.len()];

    #[cfg(feature = "parallel")]
    {
        let num_threads = rayon::current_num_threads();
        let chunk_size = (r_powers.len() + num_threads - 1) / num_threads;

        let intermediate_weights: Vec<_> = r_powers
            .par_chunks(chunk_size)
            .zip(commitment_indices.par_chunks(chunk_size))
            .map(|(r_chunk, idx_chunk)| {
                let mut local_weights = vec![B::Fr::zero(); commitments.len()];
                for (r_power, &index) in r_chunk.iter().zip(idx_chunk.iter()) {
                    local_weights[index] = local_weights[index].add(r_power);
                }
                local_weights
            })
            .collect();

        for local_weights in intermediate_weights {
            for (i, weight) in local_weights.into_iter().enumerate() {
                commitment_weights[i] = commitment_weights[i].add(&weight);
            }
        }
    }

    #[cfg(not(feature = "parallel"))]
    {
        for i in 0..r_powers.len() {
            commitment_weights[commitment_indices[i]] =
                commitment_weights[commitment_indices[i]].add(&r_powers[i]);
        }
    }

    B::G1::g1_lincomb(commitments, &commitment_weights, commitments.len(), None)
}

fn get_inv_coset_shift_for_cell<const FIELD_ELEMENTS_PER_CELL: usize, B: EcBackend, P: Preset>(
    cell_index: usize,
    fft_settings: &B::FFTSettings,
) -> Result<B::Fr, String> {
    /*
     * Get the cell index in reverse-bit order.
     * This index points to this cell's coset factor h_k in the roots_of_unity array.
     */
    let cell_index_rbl = CELL_INDICES_RBL[cell_index];

    /*
     * Observe that for every element in roots_of_unity, we can find its inverse by
     * accessing its reflected element.
     *
     * For example, consider a multiplicative subgroup with eight elements:
     *   roots = {w^0, w^1, w^2, ... w^7, w^0}
     * For a root of unity in roots[i], we can find its inverse in roots[-i].
     */
    if cell_index_rbl > P::FIELD_ELEMENTS_PER_EXT_BLOB {
        return Err("Invalid cell index".to_string());
    }
    let inv_coset_factor_idx = P::FIELD_ELEMENTS_PER_EXT_BLOB - cell_index_rbl;

    /* Get h_k^{-1} using the index */
    if inv_coset_factor_idx > P::FIELD_ELEMENTS_PER_EXT_BLOB {
        return Err("Invalid cell index".to_string());
    }

    Ok(fft_settings.get_roots_of_unity_at(inv_coset_factor_idx))
}

fn compute_commitment_to_aggregated_interpolation_poly<
    const FIELD_ELEMENTS_PER_CELL: usize,
    B: EcBackend,
    P: Preset,
>(
    r_powers: &[B::Fr],
    cell_indices: &[usize],
    cells: &[[B::Fr; FIELD_ELEMENTS_PER_CELL]],
    fft_settings: &B::FFTSettings,
    g1_monomial: &[B::G1],
) -> Result<B::G1, String> {
    let mut aggregated_column_cells =
        vec![B::Fr::zero(); P::CELLS_PER_EXT_BLOB * FIELD_ELEMENTS_PER_CELL];

    for (cell_index, column_index) in cell_indices.iter().enumerate() {
        for fr_index in 0..FIELD_ELEMENTS_PER_CELL {
            let original_fr = cells[cell_index].as_ref()[fr_index].clone();

            let scaled_fr = original_fr.mul(&r_powers[cell_index]);

            let array_index = column_index * FIELD_ELEMENTS_PER_CELL + fr_index;
            aggregated_column_cells[array_index] =
                aggregated_column_cells[array_index].add(&scaled_fr);
        }
    }

    let mut is_cell_used = vec![false; P::CELLS_PER_EXT_BLOB];

    for cell_index in cell_indices {
        is_cell_used[*cell_index] = true;
    }

    let mut aggregated_interpolation_poly = vec![B::Fr::zero(); FIELD_ELEMENTS_PER_CELL];
    for (i, is_cell_used) in is_cell_used.iter().enumerate() {
        if !is_cell_used {
            continue;
        }

        let index = i * FIELD_ELEMENTS_PER_CELL;

        reverse_bit_order(&mut aggregated_column_cells[index..(index + FIELD_ELEMENTS_PER_CELL)])?;

        let mut column_interpolation_poly = fft_settings.fft_fr(
            &aggregated_column_cells[index..(index + FIELD_ELEMENTS_PER_CELL)],
            true,
        )?;

        let inv_coset_factor =
            get_inv_coset_shift_for_cell::<FIELD_ELEMENTS_PER_CELL, B, P>(i, fft_settings)?;

        shift_poly::<B>(&mut column_interpolation_poly, &inv_coset_factor);

        for k in 0..FIELD_ELEMENTS_PER_CELL {
            aggregated_interpolation_poly[k] =
                aggregated_interpolation_poly[k].add(&column_interpolation_poly[k]);
        }
    }

    // TODO: maybe pass precomputation here?
    Ok(B::G1::g1_lincomb(
        g1_monomial,
        &aggregated_interpolation_poly,
        FIELD_ELEMENTS_PER_CELL,
        None,
    ))
}

fn get_coset_shift_pow_for_cell<const FIELD_ELEMENTS_PER_CELL: usize, B: EcBackend, P: Preset>(
    cell_index: usize,
    fft_settings: &B::FFTSettings,
) -> Result<B::Fr, String> {
    /*
     * Get the cell index in reverse-bit order.
     * This index points to this cell's coset factor h_k in the roots_of_unity array.
     */
    let cell_idx_rbl = CELL_INDICES_RBL[cell_index];

    /*
     * Get the index to h_k^n in the roots_of_unity array.
     *
     * Multiplying the index of h_k by n, effectively raises h_k to the n-th power,
     * because advancing in the roots_of_unity array corresponds to increasing exponents.
     */
    let h_k_pow_idx = cell_idx_rbl * FIELD_ELEMENTS_PER_CELL;

    if h_k_pow_idx > P::FIELD_ELEMENTS_PER_EXT_BLOB {
        return Err("Invalid cell index".to_string());
    }

    /* Get h_k^n using the index */
    Ok(fft_settings.get_roots_of_unity_at(h_k_pow_idx))
}

fn computed_weighted_sum_of_proofs<
    const FIELD_ELEMENTS_PER_CELL: usize,
    B: EcBackend,
    P: Preset,
>(
    proofs: &[B::G1],
    r_powers: &[B::Fr],
    cell_indices: &[usize],
    fft_settings: &B::FFTSettings,
) -> Result<B::G1, String> {
    let num_cells = proofs.len();

    if r_powers.len() != num_cells || cell_indices.len() != num_cells {
        return Err("Length mismatch".to_string());
    }

    let mut weighted_powers_of_r = Vec::with_capacity(num_cells);
    for i in 0..num_cells {
        let h_k_pow = get_coset_shift_pow_for_cell::<FIELD_ELEMENTS_PER_CELL, B, P>(
            cell_indices[i],
            fft_settings,
        )?;

        weighted_powers_of_r.push(r_powers[i].mul(&h_k_pow));
    }

    Ok(B::G1::g1_lincomb(
        proofs,
        &weighted_powers_of_r,
        num_cells,
        None,
    ))
}

/*
 * Automatically implement DAS for all backends
 */
impl<B: EcBackend, const FIELD_ELEMENTS_PER_CELL: usize, P: Preset>
    DAS<B, FIELD_ELEMENTS_PER_CELL, P> for B::KZGSettings
{
    fn kzg_settings(&self) -> &<B as EcBackend>::KZGSettings {
        self
    }
}
