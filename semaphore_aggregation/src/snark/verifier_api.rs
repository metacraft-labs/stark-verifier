use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use crate::ProofTuple;
use anyhow::Context;
use colored::Colorize;
use halo2_kzg_srs::{Srs, SrsFormat};
use halo2_proofs::dev::MockProver;
use halo2_proofs::halo2curves::bn256::{Bn256, Fq, Fr, G1Affine};
use halo2_proofs::plonk::{
    create_proof, keygen_pk, keygen_vk, verify_proof, Circuit, ProvingKey, VerifyingKey,
};
use halo2_proofs::poly::commitment::{Params, ParamsProver};
use halo2_proofs::poly::kzg::commitment::{KZGCommitmentScheme, ParamsKZG};
use halo2_proofs::poly::kzg::multiopen::{ProverGWC, VerifierGWC};
use halo2_proofs::poly::kzg::strategy::AccumulatorStrategy;
use halo2_proofs::poly::VerificationStrategy;
use halo2_proofs::transcript::{TranscriptReadBuffer, TranscriptWriterBuffer};
use halo2curves::goldilocks::fp::Goldilocks;
use halo2wrong_maingate::{big_to_fe, fe_to_big};
use itertools::Itertools;
use lazy_static::lazy_static;
use plonky2::{field::goldilocks_field::GoldilocksField, plonk::config::PoseidonGoldilocksConfig};
use poseidon::Spec;
use rand::rngs::OsRng;
use snark_verifier::loader::evm::{self, encode_calldata, EvmLoader, ExecutorBuilder};
use snark_verifier::pcs::kzg::{Gwc19, KzgAs};
use snark_verifier::system::halo2::transcript::evm::EvmTranscript;
use snark_verifier::system::halo2::{compile, Config};
use snark_verifier::verifier::{self, SnarkVerifier};

use super::types::{
    self, common_data::CommonData, proof::ProofValues, verification_key::VerificationKeyValues,
};
use super::verifier_circuit::Verifier;

type PlonkVerifier = verifier::plonk::PlonkVerifier<KzgAs<Bn256, Gwc19>>;

lazy_static! {
    static ref SRS: ParamsKZG<Bn256> = EvmVerifier::gen_srs(23);
}

struct EvmVerifier {}

impl EvmVerifier {
    pub fn gen_srs(k: u32) -> ParamsKZG<Bn256> {
        ParamsKZG::<Bn256>::setup(k, OsRng)
    }

    fn prepare_params(path: PathBuf) -> ParamsKZG<Bn256> {
        let srs = Srs::<Bn256>::read(
            &mut std::fs::File::open(path.clone())
                .with_context(|| format!("Failed to read .srs file {}", path.to_str().unwrap()))
                .unwrap(),
            SrsFormat::PerpetualPowerOfTau(23),
        );

        let mut buf = Vec::new();
        srs.write_raw(&mut buf);
        let params = ParamsKZG::<Bn256>::read(&mut std::io::Cursor::new(buf))
            .with_context(|| "Malformed params file")
            .unwrap();
        params
    }

    fn gen_pk<C: Circuit<Fr>>(params: &ParamsKZG<Bn256>, circuit: &C) -> ProvingKey<G1Affine> {
        let vk = keygen_vk(params, circuit).unwrap();
        keygen_pk(params, vk, circuit).unwrap()
    }

    fn gen_proof<C: Circuit<Fr>>(
        params: &ParamsKZG<Bn256>,
        pk: &ProvingKey<G1Affine>,
        circuit: C,
        instances: Vec<Vec<Fr>>,
    ) -> Vec<u8> {
        MockProver::run(params.k(), &circuit, instances.clone())
            .unwrap()
            .assert_satisfied();

        let instances = instances
            .iter()
            .map(|instances| instances.as_slice())
            .collect_vec();
        let proof = {
            let mut transcript = TranscriptWriterBuffer::<_, G1Affine, _>::init(Vec::new());
            create_proof::<
                KZGCommitmentScheme<Bn256>,
                ProverGWC<_>,
                _,
                _,
                EvmTranscript<_, _, _, _>,
                _,
            >(
                params,
                pk,
                &[circuit],
                &[instances.as_slice()],
                OsRng,
                &mut transcript,
            )
            .unwrap();
            transcript.finalize()
        };

        let accept = {
            let mut transcript = TranscriptReadBuffer::<_, G1Affine, _>::init(proof.as_slice());
            VerificationStrategy::<_, VerifierGWC<_>>::finalize(
                verify_proof::<_, VerifierGWC<_>, _, EvmTranscript<_, _, _, _>, _>(
                    params.verifier_params(),
                    pk.get_vk(),
                    AccumulatorStrategy::new(params.verifier_params()),
                    &[instances.as_slice()],
                    &mut transcript,
                )
                .unwrap(),
            )
        };
        assert!(accept);

        proof
    }

    /// Generates EVM verifier for the proof generated by circuit `stark_verifier`
    fn gen_evm_verifier(
        params: &ParamsKZG<Bn256>,
        vk: &VerifyingKey<G1Affine>,
        num_instance: Vec<usize>,
    ) -> Vec<u8> {
        let protocol = compile(
            params,
            vk,
            Config::kzg().with_num_instance(num_instance.clone()),
        );
        let vk = (params.get_g()[0], params.g2(), params.s_g2()).into();

        let loader = EvmLoader::new::<Fq, Fr>();
        let protocol = protocol.loaded(&loader);
        let mut transcript = EvmTranscript::<_, Rc<EvmLoader>, _, _>::new(&loader);

        let instances = transcript.load_instances(num_instance);
        let proof = PlonkVerifier::read_proof(&vk, &protocol, &instances, &mut transcript).unwrap();
        PlonkVerifier::verify(&vk, &protocol, &instances, &proof).unwrap();

        evm::compile_yul(&loader.yul_code())
    }

    fn evm_verify(deployment_code: Vec<u8>, instances: Vec<Vec<Fr>>, proof: Vec<u8>) {
        let calldata = encode_calldata(&instances, &proof);
        let success = {
            let mut evm = ExecutorBuilder::default()
                .with_gas_limit(u64::MAX.into())
                .build();

            let caller = evm::Address::from_low_u64_be(0xfe);
            let verifier = evm
                .deploy(caller, deployment_code.into(), 0.into())
                .address
                .unwrap();
            let result = evm.call_raw(caller, verifier, calldata.into(), 0.into());

            dbg!(result.gas_used);

            !result.reverted
        };
        assert!(success);
    }
}

fn report_elapsed(now: Instant) {
    println!(
        "{}",
        format!("Took {} milliseconds", now.elapsed().as_millis())
            .blue()
            .bold()
    );
}

/// Public API for generating Halo2 proof for Plonky2 verifier circuit
/// feed Plonky2 proof, `VerifierOnlyCircuitData`, `CommonCircuitData`
/// This runs only mock prover for constraint check
pub fn verify_inside_snark_mock(proof: ProofTuple<GoldilocksField, PoseidonGoldilocksConfig, 2>) {
    let (proof_with_public_inputs, vd, cd) = proof;

    // proof_with_public_inputs -> ProofValues type
    let proof = ProofValues::<Fr, 2>::from(proof_with_public_inputs.proof);

    let instances = proof_with_public_inputs
        .public_inputs
        .iter()
        .map(|e| big_to_fe(fe_to_big::<Goldilocks>(types::to_goldilocks(*e))))
        .collect::<Vec<Fr>>();
    let vk = VerificationKeyValues::from(vd.clone());
    let common_data = CommonData::from(cd);

    let spec = Spec::<Goldilocks, 12, 11>::new(8, 22);

    let verifier_circuit = Verifier::new(proof, instances.clone(), vk, common_data, spec);
    let _prover = MockProver::run(23, &verifier_circuit, vec![instances]).unwrap();
    _prover.assert_satisfied()
}

/// Public API for generating Halo2 proof for Plonky2 verifier circuit
/// feed Plonky2 proof, `VerifierOnlyCircuitData`, `CommonCircuitData`
/// This runs real prover and generates valid SNARK proof, generates EVM verifier and runs the verifier
pub fn verify_inside_snark(proof: ProofTuple<GoldilocksField, PoseidonGoldilocksConfig, 2>) {
    let (proof_with_public_inputs, vd, cd) = proof;
    let proof = ProofValues::<Fr, 2>::from(proof_with_public_inputs.proof);
    let instances = proof_with_public_inputs
        .public_inputs
        .iter()
        .map(|e| big_to_fe(fe_to_big::<Goldilocks>(types::to_goldilocks(*e))))
        .collect::<Vec<Fr>>();
    let vk = VerificationKeyValues::from(vd.clone());
    let common_data = CommonData::from(cd);
    let spec = Spec::<Goldilocks, 12, 11>::new(8, 22);

    // runs mock prover
    let circuit = Verifier::new(proof, instances.clone(), vk, common_data, spec);
    let mock_prover = MockProver::run(22, &circuit, vec![instances.clone()]).unwrap();
    mock_prover.assert_satisfied();
    println!("{}", "Mock prover passes".white().bold());

    // generates EVM verifier
    let pk = EvmVerifier::gen_pk(&SRS, &circuit);
    let deployment_code = EvmVerifier::gen_evm_verifier(&SRS, pk.get_vk(), vec![instances.len()]);

    // generates SNARK proof and runs EVM verifier
    println!("{}", "Starting finalization phase".red().bold());
    let now = Instant::now();
    let proof = EvmVerifier::gen_proof(&SRS, &pk, circuit.clone(), vec![instances.clone()]);
    println!("{}", "SNARK proof generated successfully!".white().bold());
    report_elapsed(now);
    EvmVerifier::evm_verify(deployment_code, vec![instances], proof);
}
