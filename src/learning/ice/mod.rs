//! ICE learner.

use common::* ;
use common::data::* ;
use common::msg::* ;

use self::smt::* ;

pub mod quals ;

use self::quals::{ Qualifiers, Qual } ;


/// Launcher.
pub struct Launcher ;
unsafe impl Sync for Launcher {}
unsafe impl Send for Launcher {}
impl Launcher {
  /// Launches an smt learner.
  pub fn launch(
    core: & LearnerCore, instance: Arc<Instance>, data: Data
  ) -> Res<()> {
    use rsmt2::{ solver, Kid } ;
    let mut kid = Kid::new( conf.solver.conf() ).chain_err(
      || "while spawning the teacher's solver"
    ) ? ;
    let conflict_solver = solver(& mut kid, Parser).chain_err(
      || "while constructing the teacher's solver"
    ) ? ;
    // let mut synth_kid = Kid::new( conf.solver.conf() ).chain_err(
    //   || "while spawning the teacher's synthesis solver"
    // ) ? ;
    // let synth_solver = solver(& mut synth_kid, Parser).chain_err(
    //   || "while constructing the teacher's synthesis solver"
    // ) ? ;
    if let Some(log) = conf.solver.log_file("ice_learner") ? {
      // let synth_log = conf.solver.log_file("ice_learner_synth")?.expect(
      //   "[unreachable] log mod is active"
      // ) ;
      IceLearner::new(
        & core, instance, data,
        conflict_solver.tee(log), // synth_solver.tee(synth_log)
      ).chain_err(
        || "while creating ice learner"
      )?.run()
    } else {
      IceLearner::new(
        & core, instance, data, conflict_solver, // synth_solver
      ).chain_err(
        || "while creating ice learner"
      )?.run()
    }
  }
}
impl Learner for Launcher {
  fn run(
    & self, core: LearnerCore, instance: Arc<Instance>, data: Data
  ) {
    if let Err(e) = Self::launch(& core, instance, data) {
      let _ = core.err(e) ;
    }
  }
  fn description(& self) -> String {
    "ice".into()
  }
}

/// Ice learner.
pub struct IceLearner<'core, Slver> {
  /// Arc to the instance.
  pub instance: Arc<Instance>,
  /// Qualifiers for the predicates.
  pub qualifiers: Qualifiers,
  /// Current data.
  data: Data,
  /// Solver used to check if the constraints are respected.
  solver: Slver,
  // /// Solver used to synthesize an hyperplane separating two points.
  // synth_solver: Slver,
  /// Learner core.
  core: & 'core LearnerCore,
  /// Branches of the tree, used when constructing a decision tree.
  finished: Vec<Branch>,
  /// Branches to construct later, used when constructing a decision tree.
  unfinished: Vec< (Branch, CData) >,
  /// Classifier for constraint data.
  classifier: HashMap<HSample, bool>,
  /// Declaration memory: used when declaring samples in the solver to
  /// remember what's already declared. The `u64` is the sample's uid.
  dec_mem: PrdMap< HashSet<u64> >,
  /// Current candidate, cleared at the beginning of each learning phase.
  candidate: PrdMap< Option<Term> >,
  /// Profiler.
  _profiler: Profiler,
  /// Vector used during learning, avoids re-allocation.
  predicates: Vec<(usize, usize, PrdIdx)>,
  /// Rng to decide when to sort predicates.
  rng: ::rand::StdRng,
}
impl<'core, 'kid, Slver> IceLearner<'core, Slver>
where Slver: Solver<'kid, Parser> {
  /// Ice learner constructor.
  pub fn new(
    core: & 'core LearnerCore, instance: Arc<Instance>, data: Data,
    solver: Slver, // synth_solver: Slver
  ) -> Res<Self> {
    let _profiler = Profiler::new() ;
    profile!{ |_profiler| tick "mining" }
    let qualifiers = Qualifiers::new( instance.clone() ).chain_err(
      || "while creating qualifier structure"
    ) ? ;

    profile!{ |_profiler| mark "mining" }
    // if_verb!{
    //   log_info!{ "qualifiers:" } ;
    //   for quals in qualifiers.qualifiers() {
    //     for (qual, _) in quals {
    //       log_info!("- {}", qual)
    //     }
    //   }
    // }
    let dec_mem = vec![
      HashSet::with_capacity(103) ; instance.preds().len()
    ].into() ;
    let candidate = vec![ None ; instance.preds().len() ].into() ;
    let predicates = Vec::with_capacity( instance.preds().len() ) ;
    Ok(
      IceLearner {
        instance, qualifiers, data, solver, // synth_solver,
        core,
        finished: Vec::with_capacity(103),
        unfinished: Vec::with_capacity(103),
        classifier: HashMap::with_capacity(1003),
        dec_mem, candidate,
        _profiler,
        predicates,
        rng: {
          use rand::SeedableRng ;
          ::rand::StdRng::from_seed(& [ 42 ])
        },
      }
    )
  }

  /// Runs the learner.
  pub fn run(mut self) -> Res<()> {
    let mut teacher_alive = true ;
    profile!{ self "qualifier synthesized" => add 0 }
    profile!{ self "qualifiers initially" => add self.qualifiers.qual_count() }
    // if_verb!{
    //   teacher_alive = msg!{ & self => "Qualifiers:" } ;
    //   for quals in self.qualifiers.qualifiers() {
    //     for & (ref qual, _) in quals {
    //       teacher_alive = msg!{ & self => "  {}", qual }
    //     }
    //   }
    // }
    'learn: loop {
      if ! teacher_alive {
        bail!("teacher is dead T__T")
      }
      profile!{ self tick "waiting" }
      match self.recv() {
        Some(data) => {
          profile!{ self mark "waiting" }
          if let Some(candidates) = self.learn(data) ? {
            teacher_alive = self.send_cands(candidates) ?
          } else {
            bail!("can't synthesize candidates for this, sorry")
          }
        },
        None => {
          profile!{ self mark "waiting" }
          return self.finalize()
        },
      }
    }
  }

  /// Finalizes the learning process.
  #[cfg( not(feature = "bench") )]
  pub fn finalize(self) -> Res<()> {
    profile!{ self "qualifiers once done" => add self.qualifiers.qual_count() }

    let success = self.core.stats(
      self._profiler, vec![ vec!["learning", "smt", "data"] ]
    ) ;
    if success {
      Ok(())
    } else {
      bail!("could not send statistics to teacher")
    }
  }
  #[cfg(feature = "bench")]
  pub fn finalize(self) -> Res<()> {
    Ok(())
  }

  /// Sends some candidates.
  ///
  /// Also resets the solver and clears declaration memory.
  pub fn send_cands(& mut self, candidates: Candidates) -> Res<bool> {
    profile!{ self tick "sending" }
    let res = self.send_candidates(
      candidates
    ) ;
    // Reset and clear declaration memory.
    self.solver.reset() ? ;
    for set in self.dec_mem.iter_mut() {
      set.clear()
    } ;
    profile!{ self mark "sending" }
    Ok(res)
  }

  // /// Applies the information in the classifier to some data.
  // pub fn classify(& self, data: & mut CData) {
  //   let mut count = 0 ;
  //   for (classified, val) in & self.classifier {
  //     let val = * val ;
  //     let classified = data.unc.take(classified) ;
  //     if let Some(sample) = classified {
  //       count += 1 ;
  //       if val {
  //         let _ = data.pos.insert( sample ) ;
  //       } else if ! val {
  //         let _ = data.neg.insert( sample ) ;
  //       }
  //     }
  //   }
  //   if count != 0 {
  //     println!("classified {} samples", count)
  //   } else {
  //     msg!{ self => "no new classification detected" } ;
  //   }
  // }

  /// Looks for a classifier.
  ///
  /// # TO DO
  ///
  /// - factor vectors created in this function to avoid reallocation
  pub fn learn(
    & mut self, mut data: Data
  ) -> Res< Option<Candidates> > {
    profile! { self tick "learning" }
    profile! { self tick "learning", "setup" }
    let _new_samples = data.drain_new_samples() ;
    self.data = data ;
    // self.qualifiers.clear_blacklist() ;
    profile!{ self mark "learning", "setup" }
    // profile!{ self tick "learning", "new sample registration" }
    // self.qualifiers.register_samples( new_samples ) ? ;
    // profile!{ self mark "learning", "new sample registration" }

    let contradiction = self.setup_solver().chain_err(
      || "while initializing the solver"
    ) ? ;

    if contradiction {
      bail!( ErrorKind::Unsat )
    }

    let prd_count = self.instance.preds().len() ;
    debug_assert!{{
      let mut okay = true ;
      for term_opt in & self.candidate {
        okay = okay && term_opt.is_none() ;
      }
      okay
    }}
    // Stores `(<unclassified_count>, <classified_count>, <prd_index>)`
    debug_assert! { self.predicates.is_empty() }

    for pred in PrdRange::zero_to(prd_count) {
      // msg!{
      //   self => "current data:\n{}", self.data.to_string_info(& ()) ?
      // } ;
      if self.instance.is_known(pred) {
        continue
      }
      let pos_len = self.data.pos[pred].len() ;
      let neg_len = self.data.neg[pred].len() ;
      let unc_len = self.data.map[pred].len() ;
      if pos_len == 0 && neg_len > 0 {
        msg!( self => "legal_pred (1)" ) ;
        // Maybe we can assert everything as negative right away?
        if self.is_legal_pred(pred, false) ? {
          msg!(
            self =>
            "{} only has negative ({}) and unclassified ({}) data\n\
            legal check ok, assuming everything negative",
            self.instance[pred], neg_len, unc_len
          ) ;
          self.candidate[pred] = Some( term::fls() ) ;
          profile!{ self tick "learning", "data" }
          self.data.pred_all_false(pred) ? ;
          profile!{ self mark "learning", "data" }
          continue
        }
      }
      if neg_len == 0 && pos_len > 0 {
        msg!( self => "legal_pred (2)" ) ;
        // Maybe we can assert everything as positive right away?
        if self.is_legal_pred(pred, true) ? {
          msg!(
            self =>
            "{} only has positive ({}) and unclassified ({}) data\n\
            legal check ok, assuming everything positive",
            self.instance[pred], pos_len, unc_len
          ) ;
          self.candidate[pred] = Some( term::tru() ) ;
          self.data.pred_all_true(pred) ? ;
          continue
        }
      }
      self.predicates.push((
        unc_len, pos_len + neg_len, pred
      ))
    }

    use rand::Rng ;
    // Use simple entropy 30% of the time.
    let simple = conf.ice.simple_gain || self.rng.next_f64() <= 0.30 ;
    msg!{ self => "looking for qualifier (simple: {})...", simple } ;

    // Sort the predicates 70% of the time.
    if conf.ice.sort_preds && self.rng.next_f64() <= 0.70 {

      profile!{ self tick "learning", "predicate sorting" }
      self.predicates.sort_unstable_by(
        |
          & (
            unclassed_1, classed_1, _
          ), & (
            unclassed_2, classed_2, _
          )
        | {
          use std::cmp::Ordering::* ;
          match (unclassed_1, unclassed_2) {
            (0, 0) => classed_1.cmp(& classed_2).reverse(),
            (0, _) => Less,
            (_, 0) => Greater,
            (_, _) => match classed_1.cmp(& classed_2).reverse() {
              Equal => unclassed_1.cmp(& unclassed_2),
              res => res,
            },
          }
        }
      ) ;
      profile!{ self mark "learning", "predicate sorting" }

    } else {

      // Not sorting, forcing random order.
      profile!{ self tick "learning", "predicate sorting" }
      let mut rng = self.rng.clone() ;
      self.predicates.sort_unstable_by(
        |_, _| {
          use std::cmp::Ordering::* ;
          let rand = rng.next_f64() ;
          if rand <= 0.33 {
            Less
          } else if rand <= 0.66 {
            Equal
          } else {
            Greater
          }
        }
      ) ;
      profile!{ self mark "learning", "predicate sorting" }

    }

    'pred_iter: while let Some(
      (_unc, _cla, pred)
    ) = self.predicates.pop() {
      msg!(
        self => "{}: {} unclassified, {} classified",
                self.instance[pred], _unc, _cla
      ) ;
      let data = self.data.data_of(pred) ;
      if let Some(term) = self.pred_learn(
        pred, data, simple
      ) ? {
        self.candidate[pred] = Some(term)
      } else {
        return Ok(None)
      }
    }
    let mut candidates: PrdMap<_> = vec![
      None ; self.instance.preds().len()
    ].into() ;
    ::std::mem::swap(& mut candidates, & mut self.candidate) ;
    profile!{ self mark "learning" }

    // if conf.ice.decay {
    //   profile!{ self tick "decay" }
    //   let _brushed = self.qualifiers.brush_quals(
    //     used_quals, conf.ice.max_decay
    //   ) ;
    //   profile!{ self "brushed qualifiers" => add _brushed }
    //   profile!{ self mark "decay" }
    // }

    Ok( Some(candidates) )
  }


  /// Backtracks to the last element of `unfinished`.
  ///
  /// - updates blacklisted qualifiers
  /// - applies the current classification to the data we're backtracking to
  ///
  /// Returns `None` iff `unfinished` was empty meaning the learning process
  /// is over.
  pub fn backtrack(& mut self, pred: PrdIdx) -> Option<(Branch, CData)> {
    profile!{ self tick "learning", "backtrack" }
    // self.qualifiers.clear_blacklist() ;
    // Backtracking or exit loop.
    if let Some( (nu_branch, mut nu_data) ) = self.unfinished.pop() {
      // Update blacklisted qualifiers.
      // for & (ref t, _) in & nu_branch {
      //   self.qualifiers.blacklist(t)
      // }
      // Update data, some previously unclassified data may be classified now.
      self.data.classify(pred, & mut nu_data) ;
      profile!{ self mark "learning", "backtrack" }
      Some( (nu_branch, nu_data) )
    } else {
      profile!{ self mark "learning", "backtrack" }
      None
    }
  }

  /// Looks for a classifier for a given predicate.
  pub fn pred_learn(
    & mut self, pred: PrdIdx, mut data: CData, simple: bool
  ) -> Res< Option<Term> > {
    debug_assert!( self.finished.is_empty() ) ;
    debug_assert!( self.unfinished.is_empty() ) ;
    self.classifier.clear() ;

    msg!(
      self => "  working on predicate {} (pos: {}, neg: {}, unc: {})",
      self.instance[pred], data.pos.len(), data.neg.len(), data.unc.len()
    ) ;

    let mut branch = Vec::with_capacity(17) ;

    'learning: loop {


      // Checking whether we can close this branch.

      if data.neg.is_empty() && self.is_legal(
        pred, & data.unc, true
      ).chain_err(|| "while checking possibility of assuming positive") ? {
        msg!(
          self =>
            "  no more negative data, is_legal check ok\n  \
            forcing {} unclassifieds positive...", data.unc.len()
        ) ;
        profile!{ self tick "learning", "data" }
        for unc in data.unc {
          // let prev = self.classifier.insert(unc, true) ;
          // debug_assert!( prev.is_none() )
          self.data.stage_pos(pred, unc)
        }
        self.data.propagate() ? ;
        profile!{ self mark "learning", "data" }
        branch.shrink_to_fit() ;
        if branch.is_empty() {
          debug_assert!( self.finished.is_empty() ) ;
          debug_assert!( self.unfinished.is_empty() ) ;
          return Ok(
            Some( term::tru() )
          )
        } else {
          self.finished.push(branch) ;
        }
        if let Some((nu_branch, nu_data)) = self.backtrack(pred) {
          branch = nu_branch ;
          data = nu_data ;
          continue 'learning
        } else {
          break 'learning
        }
      }

      if data.pos.is_empty() && self.is_legal(
        pred, & data.unc, false
      ).chain_err(|| "while checking possibility of assuming negative") ? {
        msg!(
          self =>
            "  no more positive data, is_legal check ok\n  \
            forcing {} unclassifieds negative...", data.unc.len()
        ) ;
        profile!{ self tick "learning", "data" }
        for unc in data.unc {
          // let prev = self.classifier.insert(unc, false) ;
          // debug_assert!( prev.is_none() )
          self.data.stage_neg(pred, unc)
        }
        self.data.propagate() ? ;
        profile!{ self mark "learning", "data" }
        if branch.is_empty() {
          debug_assert!( self.finished.is_empty() ) ;
          debug_assert!( self.unfinished.is_empty() ) ;
          return Ok(
            Some( term::fls() )
          )
        }
        if let Some((nu_branch, nu_data)) = self.backtrack(pred) {
          branch = nu_branch ;
          data = nu_data ;
          continue 'learning
        } else {
          break 'learning
        }
      }



      // Could not close the branch, look for a qualifier.
      profile!{ self tick "learning", "qual" }
      let (qual, q_data, nq_data) = self.get_qualifier(pred, data, simple) ? ;
      profile!{ self mark "learning", "qual" }
      // msg!{ self => "qual: {}", qual } ;
      // self.qualifiers.blacklist(& qual) ;

      // Remember the branch where qualifier is false.
      let mut nq_branch = branch.clone() ;
      nq_branch.push( (qual.clone(), false) ) ;
      self.unfinished.push( (nq_branch, nq_data) ) ;

      // Update current branch and data.
      branch.push( (qual, true) ) ;
      data = q_data ;

      // Keep going.
    }

    profile!{ self tick "learning", "pred finalize" }
    debug_assert!( self.unfinished.is_empty() ) ;
    let mut or_args = Vec::with_capacity( self.finished.len() ) ;
    for branch in self.finished.drain(0..) {
      let mut and_args = Vec::with_capacity( branch.len() ) ;
      for (term, pos) in branch {
        if pos {
          and_args.push(term)
        } else {
          and_args.push( term::app(Op::Not, vec![term]) )
        }
      }
      or_args.push( term::app(Op::And, and_args) )
    }
    profile!{ self mark "learning", "pred finalize" }
    Ok(
      Some( term::app(Op::Or, or_args) )
    )
  }

  /// Looks for a qualifier. Requires a mutable `self` in case it needs to
  /// synthesize a qualifier.
  ///
  /// Does **not** blacklist the qualifier it returns.
  ///
  /// Be careful when modifying this function as it as a (tail-)recursive call.
  /// The recursive call is logically guaranteed not cause further calls and
  /// terminate right away. Please be careful to preserve this.
  ///
  /// The `simple` flag forces to use simple, unclassified-agnostic gain.
  pub fn get_qualifier(
    & mut self, pred: PrdIdx, data: CData, simple: bool
  ) -> Res< (Term, CData, CData) > {

    macro_rules! best_qual {
      (only new: $new:expr) => ({
        if simple {
          let res = self.qualifiers.maximize(
            pred, |qual| data.simple_gain(qual), $new
          ) ? ;
          if res.is_none() {
            let qualifiers = & mut self.qualifiers ;
            let all_data = & self.data ;
            qualifiers.maximize(
              pred, |qual| data.gain(pred, all_data, qual), false
            )
          } else {
            Ok(res)
          }
        } else {
          let qualifiers = & mut self.qualifiers ;
          let all_data = & self.data ;
          qualifiers.maximize(
            pred, |qual| data.gain(pred, all_data, qual), $new
          )
        }
      }) ;
    }

    if conf.ice.qual_print {
      self.qualifiers.log()
    }

    if let Some( (qual, _gain) ) = best_qual!(only new: false) ? {
      let (q_data, nq_data) = data.split(& qual) ;
      return Ok( (qual, q_data, nq_data) )
    }

    // Reachable only if none of our qualifiers can split the data.
    // if_verb!{
    //   let mut msg = format!(
    //     "\ncould not split remaining data for {}:\n", self.instance[pred]
    //   ) ;
    //   msg.push_str("pos (") ;
    //   for pos in & data.pos {
    //     msg.push_str( & format!("\n    {}", pos) )
    //   }
    //   msg.push_str("\n) neg (") ;
    //   for neg in & data.neg {
    //     msg.push_str( & format!("\n    {}", neg) )
    //   }
    //   msg.push_str("\n) unc (") ;
    //   for unc in & data.unc {
    //     msg.push_str( & format!("\n    {}", unc) )
    //   }
    //   msg.push_str("\n)") ;
    //   msg!{ self => msg } ;
    // }

    // Synthesize qualifier separating the data.
    profile!{ self tick "learning", "qual", "synthesis" }

    if data.pos.is_empty() && data.neg.is_empty() && data.unc.is_empty() {
      bail!("[bug] cannot synthesize qualifier based on no data")
    }

    let mut new = 0 ;

    for sample in & data.pos {
      new += self.synthesize(pred, sample) ?
    }
    for sample in & data.neg {
      new += self.synthesize(pred, sample) ?
    }
    for sample in & data.unc {
      new += self.synthesize(pred, sample) ?
    }

    profile!{ self mark "learning", "qual", "synthesis" }

    profile!{ self "qualifier synthesized" => add new }
    
    if let Some( (qual, _gain) ) = best_qual!(only new: true) ? {
      let (q_data, nq_data) = data.split(& qual) ;
      Ok( (qual, q_data, nq_data) )
    } else {
      bail!("[bug] unable to split the data after synthesis...")
    }
  }


  /// Checks whether assuming some data as positive (if `pos` is true,
  /// negative otherwise) is legal.
  ///
  /// **NB**: if assuming the data positive / negative is legal,
  /// the data will be forced to be positive / negative in the solver
  /// automatically. Otherwise, the actlit is deactivated.
  pub fn is_legal(
    & mut self, pred: PrdIdx, unc: & HSamples, pos: bool
  ) -> Res<bool> {
    if unc.is_empty() { return Ok(true) }
    profile!{ self tick "learning", "smt", "legal" }

    // Wrap actlit and increment counter.
    let actlit = self.solver.get_actlit() ? ;
    let actlit = ActWrap { actlit, pred, unc, pos } ;
    self.solver.assert( & actlit ) ? ;
    let actlit = actlit.destroy() ;

    let legal = if self.solver.check_sat_act( Some(& actlit) ) ? {
      profile!{ self mark "learning", "smt", "legal" }
      true
    } else {
      profile!{ self mark "learning", "smt", "legal" }
      false
    } ;
    self.solver.set_actlit(actlit, legal) ? ;
    Ok(legal)
  }


  /// Checks whether assuming **all** the unclassified data from a predicate as
  /// `pos` is legal.
  ///
  /// **NB**: if assuming the data positive / negative is legal, the data will
  /// be forced to be positive / negative in the solver automatically.
  /// Otherwise, the actlit is deactivated (`assert (not <actlit>)`).
  pub fn is_legal_pred(
    & mut self, pred: PrdIdx, pos: bool
  ) -> Res<bool> {
    profile!{ self tick "learning", "smt", "all legal" }
    let unc = & self.data.map[pred] ;
    if unc.is_empty() {
      profile!{ self mark "learning", "smt", "all legal" }
      return Ok(true)
    }

    // Wrap actlit and increment counter.
    let actlit = self.solver.get_actlit() ? ;
    let actlit = ActWrap { actlit, pred, unc, pos } ;
    self.solver.assert( & actlit ) ? ;
    let actlit = actlit.destroy() ;

    let legal = if self.solver.check_sat_act( Some(& actlit) ) ? {
      profile!{ self mark "learning", "smt", "all legal" }
      true
    } else {
      profile!{ self mark "learning", "smt", "all legal" }
      false
    } ;
    self.solver.set_actlit(actlit, legal) ? ;
    Ok(legal)
  }


  /// Sets the solver to check that constraints are respected.
  ///
  /// Returns `true` if a contradiction was found.
  ///
  /// - **does not** reset the solver or clean declaration memory (must be
  ///   done before sending previous candidates)
  /// - **defines** pos (neg) data as `true` (`false`)
  /// - **declares** samples that neither pos nor neg
  /// - asserts constraints
  pub fn setup_solver(& mut self) -> Res<bool> {
    profile!{ self tick "learning", "smt", "setup" }
    
    // Dummy arguments used in the `define_fun` for pos (neg) data.
    let args: [ (SWrap, Typ) ; 0 ] = [] ;

    // Positive data.
    self.solver.comment("Positive data:") ? ;
    for (pred, set) in self.data.pos.index_iter() {
      for sample in set.iter() {
        let is_new = self.dec_mem[pred].insert( sample.uid() ) ;
        debug_assert!(is_new) ;
        self.solver.define_fun(
          & SWrap(pred, sample), & args, & Typ::Bool, & "true"
        ) ?
      }
    }
    // Negative data.
    self.solver.comment("Negative data:") ? ;
    for (pred, set) in self.data.neg.index_iter() {
      for sample in set.iter() {
        let is_new = self.dec_mem[pred].insert( sample.uid() ) ;
        if ! is_new {
          // Contradiction found.
          return Ok(true)
        }
        self.solver.define_fun(
          & SWrap(pred, sample), & args, & Typ::Bool, & "false"
        ) ?
      }
    }

    self.solver.comment("Sample declarations for constraints:") ? ;
    // Declare all samples used in constraints.
    for (pred, map) in self.data.map.index_iter() {
      // if let Some(term) = self.instance.term_of(pred) {
      //   if term.is_true() {
      //     self.solver.comment(
      //       & format!(
      //         "Predicate {} is forced to be `true`:", self.instance[pred]
      //       )
      //     ) ? ;
      //     for (sample, _) in map.read().map_err(corrupted_err)?.iter() {
      //       let uid = sample.uid() ;
      //       if ! self.dec_mem[pred].contains(& uid) {
      //         let _ = self.dec_mem[pred].insert(uid) ;
      //         self.solver.define_fun(
      //           & SWrap(pred, sample), & args, & Typ::Bool, & "true", & ()
      //         ) ?
      //       }
      //     }
      //   } else {
      //     bail!(
      //       "predicate {} is forced to {}, unsupported for now",
      //       self.instance[pred], term
      //     )
      //   }
      // } else {
        for (sample, _) in map.iter() {
          let uid = sample.uid() ;
          if ! self.dec_mem[pred].contains(& uid) {
            let _ = self.dec_mem[pred].insert(uid) ;
            self.solver.declare_const(
              & SWrap(pred, sample), & Typ::Bool
            ) ?
          }
        }
      // }
    }

    self.solver.comment("Constraints:") ? ;
    // Assert all constraints.
    for constraint in self.data.constraints.iter() {
      if ! constraint.is_tautology() {
        self.solver.assert( & CWrap(constraint) ) ?
      }
    }
    profile!{ self mark "learning", "smt", "setup" }

    Ok(false)
  }


  /// Qualifier synthesis, fpice style.
  pub fn synthesize(
    & mut self, pred: PrdIdx, sample: & HSample
  ) -> Res<usize> {
    let mut previous_int: Vec<(VarIdx, & Int)> = Vec::with_capacity(
      sample.len()
    ) ;

    let mut count = 0 ;
    macro_rules! insert {
      ($term:expr) => (
        // println!("synthesizing {}", $term) ;
        if self.qualifiers.insert($term, pred) ? {
          // println!("  new") ;
          count += 1
        } else {
          // println!("  not new")
        }
      ) ;
    }

    for (var_idx, val) in sample.index_iter() {
      
      match * val {
        
        Val::I(ref val) => {
          let var = term::var(var_idx) ;

          let val_term = term::int( val.clone() ) ;
          let term = term::app(
            Op::Ge, vec![ var.clone(), val_term.clone() ]
          ) ;
          insert! { & term }
          let term = term::app(
            Op::Le, vec![ var.clone(), val_term.clone() ]
          ) ;
          insert! { & term }
          let term = term::app(
            Op::Eql, vec![ var.clone(), val_term ]
          ) ;
          insert! { & term }
          for & (pre_var, pre_val) in & previous_int {
            let other_var = term::var(pre_var) ;
            if val == pre_val {
              let term = term::eq(
                var.clone(), other_var.clone()
              ) ;
              insert!{ & term }
            }
            if - val == * pre_val {
              let term = term::eq(
                var.clone(), term::sub( vec![ other_var.clone() ] )
              ) ;
              insert!{ & term }
            }

            let add = term::app(
              Op::Add, vec![ var.clone(), other_var.clone() ]
            ) ;
            let add_val = term::int( val + pre_val ) ;
            let term = term::app(
              Op::Ge, vec![ add.clone(), add_val.clone() ]
            ) ;
            insert!{ & term }
            let term = term::app(
              Op::Le, vec![ add, add_val ]
            ) ;
            insert!{ & term }

            let sub = term::app(
              Op::Sub, vec![ var.clone(), other_var.clone() ]
            ) ;
            let sub_val = term::int( val - pre_val ) ;
            let term = term::app(
              Op::Ge, vec![ sub.clone(), sub_val.clone() ]
            ) ;
            insert!{ & term }
            let term = term::app(
              Op::Le, vec![ sub, sub_val ]
            ) ;
            insert!{ & term }
          }

          previous_int.push( (var_idx, val) )
        },

        Val::B(_) => (),

        Val::N => continue,

      }

    }
    Ok(count)
  }
}

impl<
  'core, 'kid, Slver: Solver<'kid, Parser>
> HasLearnerCore for IceLearner<'core, Slver> {
  fn core(& self) -> & LearnerCore { self.core }
}






/// A branch of a decision tree.
///
/// Boolean is `false` if the term should be negated.
pub type Branch = Vec<(Term, bool)> ;

/// Projected data to classify.
#[derive(Clone)]
pub struct CData {
  /// Positive samples.
  pub pos: HSamples,
  /// Negative samples.
  pub neg: HSamples,
  /// Unclassified samples.
  pub unc: HSamples,
}
impl CData {

  /// Shannon entropy given the number of positive and negative samples.
  fn shannon_entropy(pos: f64, neg: f64) -> f64 {
    if pos == 0. && neg == 0. { return 1. }
    let den = pos + neg ;
    let (pos, neg) = (pos / den, neg / den) ;
    let (pos, neg) = (
      if pos <= 0. { 0. } else { - ( pos * pos.log2() ) },
      if neg <= 0. { 0. } else { - ( neg * neg.log2() ) }
    ) ;
    pos + neg
  }

  /// Shannon-entropy-based information gain of a qualifier (simple, ignores
  /// unclassified data).
  pub fn simple_gain(& self, qual: & mut Qual) -> Res< Option<f64> > {
    let my_entropy = Self::shannon_entropy(
      self.pos.len() as f64, self.neg.len() as f64
    ) ;
    let card = (self.pos.len() as f64) + (self.neg.len() as f64) ;
    let (
      mut q_pos, mut q_neg, mut q_unc, mut nq_pos, mut nq_neg, mut nq_unc
    ) = (0., 0., 0., 0., 0., 0.) ;
    for pos in & self.pos {
      match qual.bool_eval( pos.get() ) ? {
        Some(true) => q_pos += 1.,
        Some(false) => nq_pos += 1.,
        None => return Ok(None),
      }
    }
    for neg in & self.neg {
      match qual.bool_eval( neg.get() ) ? {
        Some(true) => q_neg += 1.,
        Some(false) => nq_neg += 1.,
        None => return Ok(None),
      }
    }
    for unc in & self.unc {
      match qual.bool_eval( unc.get() ) ? {
        Some(true) => q_unc += 1.,
        Some(false) => nq_unc += 1.,
        None => return Ok(None),
      }
    }
    if q_pos + q_neg + q_unc == 0. || nq_pos + nq_neg + nq_unc == 0. {
      Ok( None )
    } else {
      let (q_entropy, nq_entropy) = (
        Self::shannon_entropy( q_pos,  q_neg),
        Self::shannon_entropy(nq_pos, nq_neg)
      ) ;

      Ok(
        Some((
          my_entropy - (
            ( (q_pos + q_neg) *  q_entropy / card ) +
            ( (nq_pos + nq_neg) * nq_entropy / card )
          )
        ))
      )
    }
  }


  /// Modified entropy, uses [`EntropyBuilder`](struct.EntropyBuilder.html).
  ///
  /// Only takes into account unclassified data when `conf.ice.simple_gain`
  /// is false.
  pub fn entropy(& self, pred: PrdIdx, data: & Data) -> Res<f64> {
    let mut proba = EntropyBuilder::new() ;
    proba.set_pos_count( self.pos.len() ) ;
    proba.set_neg_count( self.neg.len() ) ;
    for unc in & self.unc {
      proba.add_unc(data, pred, unc) ?
    }
    Ok( proba.entropy() )
  }

  /// Modified gain, uses `entropy`.
  ///
  /// Only takes into account unclassified data when `conf.ice.simple_gain`
  /// is false.
  pub fn gain(
    & self, pred: PrdIdx, data: & Data, qual: & mut Qual
  ) -> Res< Option<f64> > {
    let my_entropy = self.entropy(pred, data) ? ;
    let my_card = (
      self.pos.len() + self.neg.len() + self.unc.len()
    ) as f64 ;
    let (mut q_ent, mut nq_ent) = (
      EntropyBuilder::new(), EntropyBuilder::new()
    ) ;
    let (
      mut q_pos, mut q_neg, mut q_unc, mut nq_pos, mut nq_neg, mut nq_unc
    ) = (0, 0, 0., 0, 0, 0.) ;
    for pos in & self.pos {
      match qual.bool_eval( pos.get() ) ? {
        Some(true) => q_pos += 1,
        Some(false) => nq_pos += 1,
        None => return Ok(None),
      }
    }
    q_ent.set_pos_count(q_pos) ;
    nq_ent.set_pos_count(nq_pos) ;

    for neg in & self.neg {
      match qual.bool_eval( neg.get() ) ? {
        Some(true) => q_neg += 1,
        Some(false) => nq_neg += 1,
        None => return Ok(None),
      }
    }
    q_ent.set_neg_count(q_neg) ;
    nq_ent.set_neg_count(nq_neg) ;

    for unc in & self.unc {
      match qual.bool_eval( unc.get() ) ? {
        Some(true) => {
          q_unc += 1. ;
          q_ent.add_unc(data, pred, unc) ?
        },
        Some(false) => {
          nq_unc += 1. ;
          nq_ent.add_unc(data, pred, unc) ?
        },
        None => return Ok(None),
      }
    }
    
    let (q_pos, q_neg, nq_pos, nq_neg) = (
      q_pos as f64, q_neg as f64, nq_pos as f64, nq_neg as f64
    ) ;

    // Is this qualifier separating anything?
    if q_pos + q_neg + q_unc == 0.
    || nq_pos + nq_neg + nq_unc == 0. {
      return Ok(None)
    }

    let (q_entropy, nq_entropy) = (q_ent.entropy(), nq_ent.entropy()) ;

    let gain = my_entropy - (
      (q_pos + q_neg + q_unc) * q_entropy / my_card +
      (nq_pos + nq_neg + nq_unc) * nq_entropy / my_card
    ) ;

    Ok( Some(gain) )
  }

  /// Splits the data given some qualifier. First is the data for which the
  /// qualifier is true.
  pub fn split(self, qual: & Term) -> (Self, Self) {
    let (mut q, mut nq) = (
      CData {
        pos: Vec::with_capacity( self.pos.len() ),
        neg: Vec::with_capacity( self.neg.len() ),
        unc: Vec::with_capacity( self.unc.len() ),
      },
      CData {
        pos: Vec::with_capacity( self.pos.len() ),
        neg: Vec::with_capacity( self.neg.len() ),
        unc: Vec::with_capacity( self.unc.len() ),
      }
    ) ;

    for pos in self.pos {
      if qual.bool_eval( pos.get() ).and_then(
        |res| res.ok_or_else(
          || ErrorKind::Msg( "model is not complete enough".into() ).into()
        )
      ).expect("error evaluating qualifier") {
        q.pos.push( pos )
      } else {
        nq.pos.push( pos )
      }
    }
    for neg in self.neg {
      if qual.bool_eval( neg.get() ).and_then(
        |res| res.ok_or_else(
          || ErrorKind::Msg( "model is not complete enough".into() ).into()
        )
      ).expect("error evaluating qualifier") {
        q.neg.push( neg )
      } else {
        nq.neg.push( neg )
      }
    }
    for unc in self.unc {
      if qual.bool_eval( unc.get() ).and_then(
        |res| res.ok_or_else(
          || ErrorKind::Msg( "model is not complete enough".into() ).into()
        )
      ).expect("error evaluating qualifier") {
        q.unc.push( unc )
      } else {
        nq.unc.push( unc )
      }
    }

    q.pos.shrink_to_fit() ;
    q.neg.shrink_to_fit() ;
    q.unc.shrink_to_fit() ;
    nq.pos.shrink_to_fit() ;
    nq.neg.shrink_to_fit() ;
    nq.unc.shrink_to_fit() ;

    (q, nq)
  }
}



/// Wrapper around an `f64` used to compute an approximation of the ratio
/// between legal positive classifications and negative ones, without actually
/// splitting the data.
///
/// See the paper for more details.
pub struct EntropyBuilder { num: f64, den: usize }
impl EntropyBuilder {
  /// Constructor.
  pub fn new() -> Self {
    EntropyBuilder { num: 0., den: 0 }
  }

  /// Sets the number of positive samples.
  pub fn set_pos_count(& mut self, pos: usize) {
    self.num += pos as f64 ;
    self.den += pos
  }
  /// Sets the number of negative samples.
  pub fn set_neg_count(& mut self, neg: usize) {
    self.den += neg
  }

  /// Adds the degree of an unclassified example.
  pub fn add_unc(
    & mut self, data: & Data, prd: PrdIdx, sample: & HSample
  ) -> Res<()> {
    self.den += 1 ;
    self.num += (1. / 2.) + (
      Self::degree(data, prd, sample) ? / ::std::f64::consts::PI
    ).atan() ;
    Ok(())
  }

  /// Probability stored in the builder.
  pub fn proba(& self) -> f64 {
    self.num / (self.den as f64)
  }

  /// Destroys the builder and returns the entropy.
  pub fn entropy(self) -> f64 {
    let proba = self.proba() ;
    let (pos, neg) = (
      if proba == 0. { 0. } else {
        proba * proba.log2()
      },
      if proba == 1. { 0. } else {
        (1. - proba) * (1. - proba).log2()
      }
    ) ;
    - pos - neg
  }

  /// Degree of a sample, refer to the paper for details.
  pub fn degree(
    data: & Data, prd: PrdIdx, sample: & HSample
  ) -> Res<f64> {
    let (
      mut sum_imp_rhs,
      mut sum_imp_lhs,
      mut sum_neg,
    ) = (0., 0., 0.) ;

    if let Some(constraints) = data.map[prd].get(& sample) {
      for constraint in constraints {
        let constraint = & data.constraints[* constraint] ;
        match constraint.rhs {
          None => sum_neg = sum_neg + 1. / (constraint.lhs.len() as f64),
          Some( Sample { pred, ref args } )
          if pred == prd
          && args == sample => sum_imp_rhs = sum_imp_rhs + 1. / (
            1. + (constraint.lhs.len() as f64)
          ),
          _ => {
            debug_assert!(
              constraint.lhs.iter().fold(
                false,
                |b, & Sample { pred, ref args }|
                  b || ( pred == prd && args == sample )
              )
            ) ;
            sum_imp_lhs = sum_imp_lhs + 1. / (
              1. + (constraint.lhs.len() as f64)
            )
          },
        }
      }
    }

    Ok(sum_imp_rhs - sum_imp_lhs - sum_neg)
  }
}








/// Smt-related things.
pub mod smt {
  use std::str::FromStr ;
  use std::io::BufRead ;

  use rsmt2::parse::{ IdentParser, ValueParser, SmtParser } ;
  use rsmt2::to_smt::* ;
  use rsmt2::actlit::Actlit ;

  use common::* ;
  use common::data::* ;



  /// Can parse values (int) and idents (`VarIdx`).
  ///
  /// In the ice learner, parsing is only used for synthesizing, not for
  /// conflict detection.
  #[derive(Clone, Copy)]
  pub struct Parser ;

  impl<'a> IdentParser<Option<VarIdx>, (), & 'a str> for Parser {
    fn parse_ident(self, input: & 'a str) -> SmtRes< Option<VarIdx> > {
      if input ==  "v" { return Ok(None) }

      debug_assert_eq!( & input[0..2], "v_" ) ;
      match usize::from_str(& input[2..]) {
        Ok(idx) => Ok( Some(idx.into()) ),
        Err(e) => bail!(
          "could not retrieve var index from `{}`: {}", input, e
        ),
      }
    }
    fn parse_type(self, _: & 'a str) -> SmtRes<()> {
      Ok(())
    }
  }

  impl<'a, Br> ValueParser<Int, & 'a mut SmtParser<Br>> for Parser
  where Br: BufRead {
    fn parse_value(self, input: & 'a mut SmtParser<Br>) -> SmtRes<Int> {
      if let Some(val) = input.try_int::<
        _, _, ::num::bigint::ParseBigIntError
      >(
        |int, pos| {
          let int = Int::from_str(int) ? ;
          Ok( if ! pos { - int } else { int } )
        }
      ) ? {
        Ok(val)
      } else {
        input.fail_with("unexpected value")
      }
    }
  }

  /// Wrapper around predicate / sample that forces smt printing.
  pub struct SWrap<'a>(pub PrdIdx, pub & 'a HSample) ;
  impl<'a> Expr2Smt<()> for SWrap<'a> {
    fn expr_to_smt2<Writer: Write>(
      & self, w: & mut Writer, _: ()
    ) -> SmtRes<()> {
      write!( w, "|p_{} {}|", self.0, self.1.uid() ) ? ;
      Ok(())
    }
  }
  impl<'a> Sym2Smt<()> for SWrap<'a> {
    fn sym_to_smt2<Writer>(
      & self, w: & mut Writer, _: ()
    ) -> SmtRes<()> where Writer: Write {
      self.expr_to_smt2(w, ())
    }
  }


  /// Wrapper around constraints that forces smt printing consistent with
  /// [`SWrap`](struct.SWrap.html).
  pub struct CWrap<'a>(pub & 'a Constraint) ;
  impl<'a> Expr2Smt<()> for CWrap<'a> {
    fn expr_to_smt2<Writer: Write>(
      & self, w: & mut Writer, _: ()
    ) -> SmtRes<()> {
      write!(w, "(=> (and") ? ;
      for lhs in & self.0.lhs {
        write!(w, " ", ) ? ;
        SWrap(lhs.pred, & lhs.args).expr_to_smt2(w, ()) ?
      }
      write!(w, ") ") ? ;
      if let Some(rhs) = self.0.rhs.as_ref() {
        SWrap(rhs.pred, & rhs.args).expr_to_smt2(w, ()) ?
      } else {
        write!(w, "false") ? ;
      }
      write!(w, ")") ? ;
      Ok(())
    }
  }

  /// Wrapper for activation literals activating samples for some predicate.
  ///
  /// `Sym2Smt` implementation just yields the actlit, used to declare said
  /// actlit. `Expr2Smt` is the actual activation expression
  ///
  /// ```bash
  /// (=> <actlit> (and <samples>))
  /// ```
  pub struct ActWrap<Samples> {
    /// Activation literal.
    pub actlit: Actlit,
    /// Predicate.
    pub pred: PrdIdx,
    /// Samples.
    pub unc: Samples,
    /// Indicates whether we're assuming the samples positive or negative.
    pub pos: bool,
  }
  impl<Samples> ActWrap<Samples> {
    /// Retrieve the actlit by destroying the wrapper.
    pub fn destroy(self) -> Actlit { self.actlit }
  }
  impl<'a> Expr2Smt<()> for ActWrap<& 'a HSamples> {
    fn expr_to_smt2<Writer: Write>(
      & self, w: & mut Writer, _: ()
    ) -> SmtRes<()> {
      write!(w, "(=> ") ? ;
      self.actlit.write(w) ? ;
      write!(
        w, " ({}", if self.pos { "and" } else { "not (or" }
      ) ? ;
      for unc in self.unc {
        write!(w, " ", ) ? ;
        SWrap(self.pred, unc).expr_to_smt2(w, ()) ?
      }
      write!(w, "))") ? ;
      if ! self.pos {
        write!(w, ")") ?
      }
      Ok(())
    }
  }
  impl<'a, T> Expr2Smt<()> for ActWrap<
    & 'a HConMap<HSample, T>
  > {
    fn expr_to_smt2<Writer: Write>(
      & self, w: & mut Writer, _: ()
    ) -> SmtRes<()> {
      write!(w, "(=> ") ? ;
      self.actlit.write(w) ? ;
      write!(
        w, " ({}", if self.pos { "and" } else { "not (or" }
      ) ? ;
      for (unc, _) in self.unc {
        write!(w, " ", ) ? ;
        SWrap(self.pred, unc).expr_to_smt2(w, ()) ?
      }
      write!(w, "))") ? ;
      if ! self.pos {
        write!(w, ")") ?
      }
      Ok(())
    }
  }


  /// Wrapper around some values and some coefficients, used by
  /// [synthesize](../struct.IceLearner.html#method.synthesize) to assert the
  /// constraints on its points.
  ///
  /// The expression it encodes is
  ///
  /// ```bash
  /// v_1 * c_1 + ... + v_n * c_n + self.cst >= 0 # if `self.pos`
  /// v_1 * c_1 + ... + v_n * c_n + self.cst  < 0 # otherwise
  /// ```
  ///
  /// where `[ v_1, ..., v_n ] = self.vals` and
  /// `[ c_1, ..., c_n ] = self.coefs`.
  pub struct ValCoefWrap<'a> {
    /// Values.
    pub vals: & 'a Vec<Int>,
    /// Coefficients.
    pub coefs: & 'a Vec<VarIdx>,
    /// Constant.
    pub cst: & 'static str,
    /// Positivity of the values.
    pub pos: bool,
  }
  impl<'a> ValCoefWrap<'a> {
    /// Constructor.
    pub fn new(
      vals: & 'a Vec<Int>, coefs: & 'a Vec<VarIdx>,
      cst: & 'static str, pos: bool
    ) -> Self {
      debug_assert!( vals.len() == coefs.len() ) ;
      ValCoefWrap { vals, coefs, cst, pos }
    }
  }
  impl<'a> Expr2Smt<()> for ValCoefWrap<'a> {
    fn expr_to_smt2<Writer>(
      & self, w: & mut Writer, _: ()
    ) -> SmtRes<()> where Writer: Write {
      if self.pos { write!(w, "(>= (+") } else { write!(w, "(< (+") } ? ;
      for (val, coef) in self.vals.iter().zip( self.coefs ) {
        write!(w, " (* {} ", val) ? ;
        coef.sym_to_smt2(w, ()) ? ;
        write!(w, ")") ?
      }
      write!(w, " {}) 0)", self.cst) ? ;
      Ok(())
    }
  }
}
